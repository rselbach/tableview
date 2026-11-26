use std::borrow::Cow;

use rune::{Align, Cursor, Cx, Drag, El, Phase, ScrollSpec, Tx, col, el, row, spacer, text, text_input};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{
    Button, ButtonVariant, Cell, Checkbox, ControlSize, Dialog, Field, Kit, Placement, RollingText, Select,
    SelectAppearance, Spinner, Table, TableColumn, TextInput,
};

use crate::icons;
use crate::state::{
    AppState, OverlayState, QUERY_LIMIT_OPTIONS, QueryResultTabState, QueryTabState, SaveQueryModalState, SessionState,
};
use crate::views::explain_view;
use crate::views::pill_badge;
use crate::views::table_grid::{initial_col_width, is_numeric_udt, render_cell_value};

/// Builds the full SQL query workbench tab: action bar, syntax-highlighted editor,
/// draggable vertical splitter, and multi-statement result grids.
pub fn build_query_tab(
    s: &AppState,
    _session: &SessionState,
    tab: &QueryTabState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let action_bar = build_action_bar(tab, k, layer);

    let split_key = format!("query-split-body-{}", tab.tab_id.get());
    let total_h = cx.layout_size(&split_key).map_or(540.0, |sz| sz[1]).max(260.0);
    let max_editor_h = (total_h - 148.0).max(110.0);
    let editor_h = (total_h * tab.split_ratio).clamp(110.0, max_editor_h);

    let editor_pane = build_editor_pane(tab, editor_h, k);
    let splitter = build_split_handle(total_h, k);
    let results_pane = build_results_pane(s, tab, k);

    let split_body = col()
        .key(split_key)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(editor_pane)
        .child(splitter)
        .child(results_pane);

    col()
        .key(format!("query-tab-root-{}", tab.tab_id.get()))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(action_bar)
        .child(split_body)
}

fn build_action_bar(tab: &QueryTabState, k: &Kit, layer: &mut Vec<El>) -> El {
    let c = &k.c;
    let (line, col_num) = tab.cursor_line_col();
    let timer_font = k.ty.small.mono().features("'tnum'");

    let mut left = row()
        .gap(8.0)
        .center_y()
        .child(
            text(format!("Ln {line}, Col {col_num}"))
                .key("query-cursor-pos")
                .font(timer_font)
                .color(c.text_muted),
        );

    if tab.is_dirty() {
        left = left.child(pill_badge(
            "query/dirty-badge",
            "Unsaved",
            c.layer_02,
            c.warning.fg,
            c.border_muted,
            k,
        ));
    }

    if tab.running {
        let elapsed_str = format!("{:.1}s", tab.elapsed_ms as f64 / 1000.0);
        let tick_val = (tab.elapsed_ms / 100) as f64;
        left = left
            .child(Spinner::new().size(13.0).color(c.text_accent).build(k))
            .child(
                RollingText::new(format!("q-timer-{}", tab.tab_id.get()), elapsed_str)
                    .font(timer_font)
                    .color(c.text_base)
                    .value(tick_val)
                    .trailing()
                    .build(k),
            )
            .child(
                Button::new(if tab.cancelling { "Cancelling…" } else { "Cancel" })
                    .variant(ButtonVariant::Danger)
                    .control_size(ControlSize::Small)
                    .icon(&icons::STOP_SQUARE)
                    .shortcut("mod+period")
                    .disabled(tab.cancelling)
                    .on_press(|s: &mut AppState| {
                        s.cancel_active_query();
                    })
                    .build(k)
                    .key("query-cancel-btn"),
            );
    }

    let limit_labels: Vec<&str> = QUERY_LIMIT_OPTIONS.iter().map(|(_, label)| *label).collect();
    let limit_select = Select::new(format!("query-limit-{}", tab.tab_id.get()), &limit_labels)
        .label("Query row limit")
        .appearance(SelectAppearance::Inline)
        .placement(Placement::BottomEnd)
        .current(Some(tab.row_limit_label()))
        .open(tab.limit_dropdown_open)
        .on_toggle(|s: &mut AppState| {
            if let Some(q) = s.active_session_mut().and_then(SessionState::active_query_tab_mut) {
                q.limit_dropdown_open = !q.limit_dropdown_open;
            }
        })
        .on_close(|s: &mut AppState| {
            if let Some(q) = s.active_session_mut().and_then(SessionState::active_query_tab_mut) {
                q.limit_dropdown_open = false;
            }
        })
        .on_choose(|s: &mut AppState, val, _| {
            let chosen = QUERY_LIMIT_OPTIONS
                .iter()
                .find(|(_, label)| *label == val)
                .map_or(Some(1000), |(lim, _)| *lim);
            s.set_active_query_row_limit(chosen);
        })
        .build(k, layer);

    let save_label = if tab.is_dirty() { "Save •" } else { "Save" };

    row()
        .key("query-action-bar")
        .w_full()
        .h(38.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(left)
        .child(spacer())
        .child(
            row()
                .gap(6.0)
                .center_y()
                .child(limit_select)
                .child(
                    Button::new("Explain")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .icon(&icons::EXPLAIN_TREE)
                        .shortcut("mod+shift+e")
                        .disabled(tab.running || tab.explain_loading)
                        .on_press(|s: &mut AppState| {
                            s.run_active_query_explain(false);
                        })
                        .build(k)
                        .key("query-explain-btn"),
                )
                .child(
                    Button::new("Format")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .shortcut("mod+shift+f")
                        .disabled(tab.running)
                        .on_press(|s: &mut AppState| {
                            s.format_active_query_sql();
                        })
                        .build(k)
                        .key("query-format-btn"),
                )
                .child(
                    Button::new(save_label)
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .icon(&icons::BOOKMARK)
                        .shortcut("mod+s")
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                            s.open_save_query_modal(tx);
                        })
                        .build(k)
                        .key("query-save-btn"),
                )
                .child(
                    Button::new("Run All")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .icon(&icons::PLAY)
                        .shortcut("mod+shift+enter")
                        .disabled(tab.running)
                        .on_press(|s: &mut AppState| {
                            s.run_active_query(true);
                        })
                        .build(k)
                        .key("query-run-all-btn"),
                )
                .child(
                    Button::new("Run")
                        .variant(ButtonVariant::Contrast)
                        .control_size(ControlSize::Small)
                        .icon(&icons::PLAY)
                        .shortcut("mod+enter")
                        .disabled(tab.running)
                        .on_press(|s: &mut AppState| {
                            s.run_active_query(false);
                        })
                        .build(k)
                        .key("query-run-cur-btn"),
                ),
        )
}

fn build_editor_pane(tab: &QueryTabState, editor_h: f32, k: &Kit) -> El {
    let c = &k.c;
    let code_style = k.ty.mono.lh(20.0);
    let line_h = code_style.line_px();
    let (cur_line, _) = tab.cursor_line_col();

    let line_count = if tab.editor.text.is_empty() {
        1
    } else {
        tab.editor.text.lines().count() + usize::from(tab.editor.text.ends_with('\n'))
    };

    let mut gutter = col()
        .key(format!("sql-gutter-{}", tab.tab_id.get()))
        .w(46.0)
        .py(10.0)
        .pr(8.0)
        .pl(6.0)
        .fill(c.layer_01)
        .border_sides([0.0, 1.0, 0.0, 0.0], c.border_muted);

    for n in 1..=line_count {
        gutter = gutter.child(
            row()
                .w_full()
                .h(line_h)
                .justify_end()
                .center_y()
                .child(
                    text(n.to_string())
                        .font(code_style)
                        .color(if n == cur_line { c.text_base } else { c.text_faint }),
                ),
        );
    }

    let highlighted = syntax::highlight(Some("sql"), &tab.editor.text);
    let spans = SyntaxTheme::from_kit(k).spans(&highlighted, 0);

    let editor_input = text_input(&tab.editor, code_style)
        .key(format!("sql-editor-{}", tab.tab_id.get()))
        .label("SQL query editor")
        .w_full()
        .min_h((editor_h - 22.0).max(80.0))
        .color(c.text_base)
        .caret(c.text_accent, c.text_accent.alpha(0.24))
        .placeholder("-- Write PostgreSQL SQL here (⌘↩ Run Current, ⇧⌘↩ Run All)", c.text_faint)
        .decorations(spans)
        .line_home_end(true)
        .reveal_caret_on_focus(true)
        .on_edit(AppState::active_query_editor_mut)
        .on_submit(|s: &mut AppState, _| s.insert_newline_in_active_query_editor());

    let editor_row = row()
        .w_full()
        .items_start()
        .child(gutter)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .py(10.0)
                .px(12.0)
                .child(editor_input),
        );

    let scroller = k
        .scroll_bar(
            col()
                .key(format!("sql-editor-scroll-{}", tab.tab_id.get()))
                .w_full()
                .h_full()
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(editor_row),
        )
        .w_full()
        .h_full()
        .min_h(0.0);

    col()
        .key(format!("sql-editor-pane-{}", tab.tab_id.get()))
        .w_full()
        .h(editor_h)
        .min_h(110.0)
        .fill(c.bg_base)
        .child(scroller)
}

fn build_split_handle(total_h: f32, k: &Kit) -> El {
    let c = &k.c;
    row()
        .key("sql-split-handle")
        .w_full()
        .h(6.0)
        .center()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 1.0, 0.0], c.border_muted)
        .hover(c.hover)
        .cursor(Cursor::ResizeRow)
        .on_drag(move |s: &mut AppState, d: &mut Drag, _| match d.phase {
            Phase::Down => {
                d.data = s
                    .active_session()
                    .and_then(SessionState::active_query_tab)
                    .map_or(0.42, |q| q.split_ratio);
            }
            Phase::Move | Phase::Up => {
                let next = if d.cancelled() {
                    d.data
                } else {
                    d.data + d.dy / total_h
                };
                s.set_active_query_split_ratio(next);
            }
        })
        .child(el().size(36.0, 2.0).radius(1.0).fill(c.border_strong))
}

fn build_results_pane(s: &AppState, tab: &QueryTabState, k: &Kit) -> El {
    let c = &k.c;
    let header = build_results_header(tab, k);

    let body = if tab.show_explain {
        explain_view::build_explain_pane(tab, k)
    } else if let Some(err) = &tab.batch_error
        && tab.results.is_empty()
    {
        build_error_card(
            if tab.batch_cancelled {
                "Query Cancelled"
            } else {
                "Execution Failed"
            },
            err,
            None,
            tab.batch_cancelled,
            k,
        )
    } else if let Some(res) = tab.active_result_state() {
        if let Some(err) = &res.statement.error {
            build_error_card(
                &format!("Statement {} Failed", tab.active_result + 1),
                err,
                Some(&res.statement.sql),
                false,
                k,
            )
        } else if res.statement.is_tabular() {
            build_result_grid(s, tab, res, k)
        } else {
            build_dml_card(res, k)
        }
    } else if tab.running {
        col()
            .key("query-results-running")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .child(Spinner::new().size(18.0).color(c.text_accent).build(k))
            .child(
                text(format!("Executing SQL… ({:.1}s)", tab.elapsed_ms as f64 / 1000.0))
                    .font(k.ty.ui)
                    .color(c.text_muted),
            )
    } else {
        col()
            .key("query-results-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(6.0)
            .padding(24.0)
            .child(text("No results yet").font(k.ty.body_medium).color(c.text_base))
            .child(
                text("Press ⌘↩ to execute the current statement, or ⇧⌘↩ to run all statements.")
                    .font(k.ty.ui_compact)
                    .color(c.text_muted),
            )
    };

    col()
        .key(format!("query-results-pane-{}", tab.tab_id.get()))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(c.bg_base)
        .child(header)
        .child(body)
}

fn build_results_header(tab: &QueryTabState, k: &Kit) -> El {
    let c = &k.c;
    let mut tabs_row = row().gap(4.0).center_y().shrink();

    let has_explain_tab = tab.explain_plan.is_some() || tab.explain_loading || tab.explain_error.is_some();

    if tab.results.is_empty() && !has_explain_tab {
        tabs_row = tabs_row.child(text("Results").font(k.ty.small_medium).color(c.text_muted));
    } else {
        for (idx, res) in tab.results.iter().enumerate() {
            let selected = !tab.show_explain && idx == tab.active_result;
            let num = idx + 1;
            let label = if res.statement.error.is_some() {
                format!("Error {num}")
            } else if res.statement.is_tabular() {
                let suffix = if res.statement.truncated { "+" } else { "" };
                format!("Result {num} ({}{suffix})", res.statement.rows.len())
            } else if let Some(n) = res.statement.rows_affected {
                if n == 1 {
                    format!("Stmt {num} (1 row)")
                } else {
                    format!("Stmt {num} ({n} rows)")
                }
            } else {
                format!("Stmt {num} (OK)")
            };
            let fg = if res.statement.error.is_some() {
                c.danger.fg
            } else if selected {
                c.text_base
            } else {
                c.text_muted
            };
            let bg = if selected { c.layer_02 } else { rune::Color::TRANSPARENT };
            let border = if selected { c.border_base } else { rune::Color::TRANSPARENT };

            tabs_row = tabs_row.child(
                row()
                    .key(format!("query-result-tab-{idx}"))
                    .role("tab")
                    .label(&label)
                    .focusable(true)
                    .h(24.0)
                    .px(9.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .fill(bg)
                    .border(0.5, border)
                    .hover(c.hover)
                    .on_press(move |s: &mut AppState| s.select_active_query_result_tab(idx))
                    .child(
                        text(&label)
                            .font(if selected { k.ty.small_medium } else { k.ty.small })
                            .color(fg),
                    ),
            );
        }
        if has_explain_tab {
            let selected = tab.show_explain;
            let label = if tab.explain_loading {
                "Plan…"
            } else if tab.explain_error.is_some() {
                "Plan Error"
            } else {
                "Plan"
            };
            let fg = if tab.explain_error.is_some() {
                c.danger.fg
            } else if selected {
                c.text_accent
            } else {
                c.text_muted
            };
            tabs_row = tabs_row.child(
                row()
                    .key("query-result-tab-explain")
                    .role("tab")
                    .label(label)
                    .focusable(true)
                    .h(24.0)
                    .px(9.0)
                    .gap(5.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .fill(if selected { c.layer_02 } else { rune::Color::TRANSPARENT })
                    .border(0.5, if selected { c.border_base } else { rune::Color::TRANSPARENT })
                    .hover(c.hover)
                    .on_press(|s: &mut AppState| s.show_active_query_explain_tab())
                    .child(rune::icon(&icons::EXPLAIN_TREE, 12.0).color(fg))
                    .child(
                        text(label)
                            .font(if selected { k.ty.small_medium } else { k.ty.small })
                            .color(fg),
                    ),
            );
        }
    }

    let mut summary_row = row().gap(8.0).center_y();
    if tab.batch_cancelled {
        summary_row = summary_row.child(pill_badge(
            "query/cancelled-badge",
            "Cancelled",
            c.layer_02,
            c.warning.fg,
            c.border_muted,
            k,
        ));
    }
    if !tab.show_explain
        && let Some(res) = tab.active_result_state()
    {
        if res.statement.truncated {
            summary_row = summary_row.child(pill_badge(
                "query/truncated-badge",
                format!(
                    "Showing {} of {} rows",
                    res.statement.rows.len(),
                    res.statement.total_rows_seen
                ),
                c.layer_02,
                c.warning.fg,
                c.border_muted,
                k,
            ));
        } else if res.statement.is_tabular() && res.statement.error.is_none() {
            summary_row = summary_row.child(
                text(format!("{} rows · {} ms", res.statement.rows.len(), res.statement.elapsed_ms))
                    .font(k.ty.small)
                    .color(c.text_muted),
            );
        }
        if res.statement.is_tabular() && res.statement.error.is_none() && !res.statement.rows.is_empty() {
            summary_row = summary_row.child(
                Button::new("Export")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Mini)
                    .icon(&icons::DOWNLOAD)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                        s.open_export_modal_for_active_tab(tx);
                    })
                    .build(k)
                    .key("query-export-btn"),
            );
        }
    } else if !tab.show_explain
        && let Some(total_ms) = tab.total_duration_ms
    {
        summary_row = summary_row.child(
            text(format!("{total_ms} ms"))
                .font(k.ty.small)
                .color(c.text_muted),
        );
    }

    row()
        .key("query-results-header")
        .w_full()
        .h(34.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(tabs_row)
        .child(spacer())
        .child(summary_row)
}

fn build_result_grid(s: &AppState, tab: &QueryTabState, res: &QueryResultTabState, k: &Kit) -> El {
    let c = &k.c;
    if res.statement.columns.is_empty() {
        return build_dml_card(res, k);
    }

    let null_bg = c.layer_02;
    let null_fg = c.text_faint;
    let null_border = c.border_muted;
    let bool_true_fg = c.success.fg;
    let muted_fg = c.text_muted;
    let small_font = k.ty.small;
    let radius_xs = k.radius.xs;

    let mut table = Table::new(
        |s: &AppState| s.active_query_result_table(),
        |s: &mut AppState| s.active_query_result_table_mut(),
        |s: &AppState| {
            s.active_session()
                .and_then(SessionState::active_query_tab)
                .and_then(QueryTabState::active_result_state)
                .map_or(0, |r| r.statement.rows.len())
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

    // Key columns by positional ID `c-{idx}` rather than column name because arbitrary SQL
    // outputs commonly contain duplicate aliases such as `?column?` or joined `id` fields.
    for (idx, col_meta) in res.statement.columns.iter().enumerate() {
        let mut column = TableColumn::new(format!("c-{idx}"), &col_meta.name)
            .width(initial_col_width(col_meta))
            .min(68.0)
            .max(640.0)
            .resizable(true);
        if is_numeric_udt(&col_meta.udt_name) {
            column = column.align(Align::End);
        }
        table = table.column(column);
    }

    let built = table
        .cell(move |s: &AppState, row_idx, col_id| {
            let Some(res) = s
                .active_session()
                .and_then(SessionState::active_query_tab)
                .and_then(QueryTabState::active_result_state)
            else {
                return Cell::text("");
            };
            if col_id == "#" {
                return Cell::text((row_idx + 1).to_string()).mono().muted();
            }
            let Some(col_idx) = col_id.strip_prefix("c-").and_then(|n| n.parse::<usize>().ok()) else {
                return Cell::text("");
            };
            let Some(val) = res.statement.rows.get(row_idx).and_then(|r| r.get(col_idx)) else {
                return Cell::text("");
            };
            render_cell_value(
                val,
                null_bg,
                null_fg,
                null_border,
                bool_true_fg,
                muted_fg,
                small_font,
                radius_xs,
            )
        })
        .zebra(true)
        .scroll_wide(true)
        .virtual_columns(true)
        .frozen_columns(1)
        .name(format!("{} result rows", tab.title))
        .on_select(|s: &mut AppState, row_id, _| s.select_active_tab_row(row_id))
        .build(s, k)
        .w_full()
        .grow(1.0)
        .min_h(0.0);

    if res.statement.rows.is_empty() {
        el().w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(built.w_full().h_full())
            .child(
                el().pin(Some(0.0), Some(32.0), Some(0.0), Some(0.0))
                    .center()
                    .child(
                        text("Query returned 0 rows")
                            .font(k.ty.ui)
                            .color(c.text_muted),
                    ),
            )
    } else {
        built
    }
}

fn build_dml_card(res: &QueryResultTabState, k: &Kit) -> El {
    let c = &k.c;
    let headline = match res.statement.rows_affected {
        Some(1) => "1 row affected".to_owned(),
        Some(n) => format!("{n} rows affected"),
        None => "Statement executed successfully".to_owned(),
    };
    let highlighted = syntax::highlight(Some("sql"), &res.statement.sql);
    let spans = SyntaxTheme::from_kit(k).spans(&highlighted, 0);

    col()
        .key("query-dml-summary")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(20.0)
        .child(
            col()
                .w_full()
                .max_w(620.0)
                .padding(14.0)
                .gap(10.0)
                .radius(k.radius.md)
                .fill(c.layer_01)
                .border(0.5, c.border_base)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(pill_badge(
                            "query/dml-ok",
                            "OK",
                            c.success.bg,
                            c.success.fg,
                            c.success.border,
                            k,
                        ))
                        .child(text(headline).font(k.ty.ui_medium).color(c.text_base))
                        .child(spacer())
                        .child(
                            text(format!("{} ms", res.statement.elapsed_ms))
                                .font(k.ty.small)
                                .color(c.text_muted),
                        ),
                )
                .child(
                    col()
                        .w_full()
                        .padding(10.0)
                        .radius(k.radius.sm)
                        .fill(c.bg_base)
                        .border(0.5, c.border_muted)
                        .child(
                            rune::rich(&res.statement.sql, spans)
                                .font(k.ty.mono)
                                .color(c.text_base)
                                .w_full()
                                .wrap(),
                        ),
                ),
        )
}

fn build_error_card(title: &str, message: &str, sql: Option<&str>, is_warning: bool, k: &Kit) -> El {
    let c = &k.c;
    let tone_fg = if is_warning { c.warning.fg } else { c.danger.fg };
    let tone_border = if is_warning { c.warning.border } else { c.danger.border };
    let mut card = col()
        .key("query-error-card")
        .w_full()
        .max_w(680.0)
        .padding(14.0)
        .gap(10.0)
        .radius(k.radius.md)
        .fill(c.layer_01)
        .border(0.5, tone_border)
        .child(text(title).font(k.ty.ui_medium).color(tone_fg))
        .child(text(message).font(k.ty.mono).color(c.text_base).w_full().wrap());

    if let Some(stmt_sql) = sql {
        card = card.child(
            col()
                .w_full()
                .padding(10.0)
                .radius(k.radius.sm)
                .fill(c.bg_base)
                .border(0.5, c.border_muted)
                .child(text(stmt_sql).font(k.ty.mono).color(c.text_muted).w_full().wrap()),
        );
    }

    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(20.0)
        .child(card)
}

/// Builds the Save SQL Query modal dialog.
pub fn build_save_query_modal(modal: &SaveQueryModalState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let name_input = TextInput::new(&modal.name)
        .label("Query name")
        .placeholder("Active Greendale Roster")
        .width(None)
        .on_edit(AppState::save_query_name_mut)
        .on_submit(|s: &mut AppState, tx| s.submit_save_query_modal(tx))
        .build(k)
        .key("save-query-name");

    let folder_input = TextInput::new(&modal.folder)
        .label("Folder")
        .placeholder("Reporting (optional)")
        .width(None)
        .on_edit(AppState::save_query_folder_mut)
        .on_submit(|s: &mut AppState, tx| s.submit_save_query_modal(tx))
        .build(k)
        .key("save-query-folder");

    let scope_check = Checkbox::new(modal.scope_all_connections)
        .label("Share across all connections")
        .on_toggle(|s: &mut AppState| {
            if let OverlayState::SaveQuery(ref mut m) = s.overlay {
                m.scope_all_connections = !m.scope_all_connections;
            }
        })
        .build(k);

    let mut body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .child(Field::new().label("Name").control(name_input).build(k))
        .child(Field::new().label("Folder").control(folder_input).build(k))
        .child(scope_check);

    if let Some(err) = &modal.error {
        body = body.child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap().w_full());
    }

    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .child(spacer())
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Regular)
                    .on_press(|s: &mut AppState| s.overlay = OverlayState::None)
                    .build(k),
            )
            .child(
                Button::new("Save Query")
                    .variant(ButtonVariant::Contrast)
                    .control_size(ControlSize::Regular)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.submit_save_query_modal(tx))
                    .build(k)
                    .key("save-query-submit-btn"),
            ),
    );

    Dialog::new("save-query-modal", "Save SQL Query")
        .open(true)
        .width(440.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}
