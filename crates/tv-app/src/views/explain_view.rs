use rune::{El, ScrollSpec, Tx, col, el, rich, row, spacer, text};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, SegmentedControl, Spinner};
use tv_core::explain::{ExplainNode, ExplainPlan};

use crate::icons;
use crate::state::QueryTabState;
use crate::views::pill_badge;

/// Builds the Visual EXPLAIN pane (operator tree + proportional bars, or formatted JSON).
pub fn build_explain_pane(tab: &QueryTabState, supports_analyze: bool, k: &Kit) -> El {
    let c = &k.c;

    if tab.explain_loading {
        return col()
            .key("explain-loading")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .child(Spinner::new().size(18.0).color(c.text_accent).build(k))
            .child(text("Running EXPLAIN…").font(k.ty.ui).color(c.text_muted));
    }

    if let Some(err) = &tab.explain_error {
        return col()
            .key("explain-error")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .padding(20.0)
            .child(
                col()
                    .w_full()
                    .max_w(680.0)
                    .padding(14.0)
                    .gap(8.0)
                    .radius(k.radius.md)
                    .fill(c.layer_01)
                    .border(0.5, c.danger.border)
                    .child(text("EXPLAIN Failed").font(k.ty.ui_medium).color(c.danger.fg))
                    .child(text(err).font(k.ty.mono).color(c.text_base).w_full().wrap()),
            );
    }

    let Some(plan) = &tab.explain_plan else {
        return col()
            .key("explain-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(8.0)
            .child(text("No execution plan captured yet").font(k.ty.body_medium).color(c.text_base))
            .child(
                text("Click Explain (⇧⌘E) to inspect the query plan.")
                    .font(k.ty.ui_compact)
                    .color(c.text_muted),
            );
    };

    let summary_bar = build_summary_bar(tab, plan, supports_analyze, k);

    let body = if tab.explain_raw_json {
        let highlights = syntax::highlight(Some("json"), &plan.raw_json);
        let spans = SyntaxTheme::from_kit(k).spans(&highlights, 0);
        k.scroll_bar(
            col()
                .key("explain-json-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .padding(12.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(
                    rich(&plan.raw_json, spans)
                        .font(k.ty.mono)
                        .color(c.text_base)
                        .w_full()
                        .wrap(),
                ),
        )
        .w_full()
        .grow(1.0)
        .min_h(0.0)
    } else {
        let mut flat = Vec::new();
        flatten_plan(&plan.root, 0, &mut flat);
        let root_time = plan.total_runtime_ms();
        let root_cost = plan.total_cost();

        let mut list = col().key("explain-nodes-list").w_full().padding(12.0).gap(6.0);
        for (idx, (depth, node)) in flat.into_iter().enumerate() {
            list = list.child(build_node_card(idx, depth, node, plan.analyzed, root_time, root_cost, k));
        }
        k.scroll_bar(
            col()
                .key("explain-tree-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(list),
        )
        .w_full()
        .grow(1.0)
        .min_h(0.0)
    };

    col()
        .key("explain-pane-root")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(summary_bar)
        .child(body)
}

fn build_summary_bar(tab: &QueryTabState, plan: &ExplainPlan, supports_analyze: bool, k: &Kit) -> El {
    let c = &k.c;
    let mono = k.ty.small.mono().features("'tnum'");
    let raw_for_copy = plan.raw_json.clone();

    let mut left = row()
        .gap(10.0)
        .center_y()
        .child(pill_badge(
            "explain/kind-badge",
            if plan.analyzed { "ANALYZE" } else { "ESTIMATED" },
            if plan.analyzed { c.info.bg } else { c.layer_02 },
            if plan.analyzed { c.info.fg } else { c.text_muted },
            if plan.analyzed { c.info.border } else { c.border_base },
            k,
        ));

    if let Some(exec_ms) = plan.execution_time_ms {
        left = left.child(
            text(format!("Exec {exec_ms:.2} ms"))
                .font(mono)
                .color(c.text_base),
        );
    }
    if let Some(plan_ms) = plan.planning_time_ms {
        left = left.child(
            text(format!("Plan {plan_ms:.2} ms"))
                .font(mono)
                .color(c.text_muted),
        );
    }
    left = left.child(
        text(format!("Cost {:.1}", plan.total_cost()))
            .font(mono)
            .color(c.text_muted),
    );
    if plan.analyzed && (plan.total_shared_hits() > 0 || plan.total_shared_reads() > 0) {
        left = left.child(
            text(format!(
                "Buffers {} hit / {} read",
                plan.total_shared_hits(),
                plan.total_shared_reads()
            ))
            .font(mono)
            .color(c.text_muted),
        );
    }

    let mode_switch = SegmentedControl::new(&["Tree", "JSON"], Some(usize::from(tab.explain_raw_json)))
        .label("Explain view mode")
        .as_tabs()
        .fit()
        .on_pick(|s: &mut crate::state::AppState, idx, _| {
            if let Some(q) = s
                .active_session_mut()
                .and_then(crate::state::SessionState::active_query_tab_mut)
            {
                q.explain_raw_json = idx == 1;
            }
        })
        .build(k)
        .key("explain-mode-switch");

    let mut bar = row()
        .key("explain-summary-bar")
        .w_full()
        .h(36.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .fill(c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(left)
        .child(spacer())
        .child(mode_switch);

    if supports_analyze {
        bar = bar.child(
            Button::new("Run ANALYZE")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Mini)
                .icon(&icons::PLAY)
                .on_press(|s: &mut crate::state::AppState| {
                    s.run_active_query_explain(true);
                })
                .build(k)
                .key("explain-analyze-btn"),
        );
    }

    bar.child(
        Button::new("Copy JSON")
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Mini)
            .icon(&icons::COPY)
            .on_press_tx(move |s: &mut crate::state::AppState, tx: &mut Tx| {
                tx.copy(raw_for_copy.clone());
                s.set_notice("Copied EXPLAIN JSON to clipboard", false);
            })
            .build(k)
            .key("explain-copy-btn"),
    )
}

fn flatten_plan<'a>(node: &'a ExplainNode, depth: usize, out: &mut Vec<(usize, &'a ExplainNode)>) {
    out.push((depth, node));
    for child in &node.children {
        flatten_plan(child, depth + 1, out);
    }
}

fn build_node_card(
    idx: usize,
    depth: usize,
    node: &ExplainNode,
    analyzed: bool,
    root_time: Option<f64>,
    root_cost: f64,
    k: &Kit,
) -> El {
    let c = &k.c;
    let share = node.share_fraction(root_time, root_cost);
    let mono = k.ty.small.mono().features("'tnum'");
    let (badge_bg, badge_fg, badge_border) = node_badge_colors(&node.node_type, k);
    let hot = share >= 0.45;
    let bar_fill = if hot {
        c.warning.fg
    } else if share >= 0.2 {
        c.text_accent
    } else {
        c.info.fg
    };

    let mut top = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(pill_badge(
            format!("explain/node-type/{idx}"),
            &node.node_type,
            badge_bg,
            badge_fg,
            badge_border,
            k,
        ));

    if let Some(join) = &node.join_type {
        top = top.child(text(format!("({join})")).font(k.ty.small).color(c.text_muted));
    }
    if let Some(target) = node.relation_display() {
        top = top.child(
            text(target)
                .font(k.ty.small_medium)
                .color(c.text_base)
                .shrink()
                .truncate(),
        );
    }
    if node.row_estimate_ratio().is_some() {
        top = top.child(pill_badge(
            format!("explain/skew/{idx}"),
            "Est Skew",
            c.warning.bg,
            c.warning.fg,
            c.warning.border,
            k,
        ));
    }

    let pct = (share * 100.0).round() as u32;
    let right_metric = if analyzed && let Some(excl_ms) = node.exclusive_time_ms() {
        let rows_str = match node.actual_rows {
            Some(ar) => format!("{ar} rows (est {})", node.plan_rows),
            None => format!("est {} rows", node.plan_rows),
        };
        format!("{excl_ms:.2} ms ({pct}%) · {rows_str}")
    } else {
        format!(
            "cost {:.1} ({pct}%) · est {} rows",
            node.exclusive_cost(),
            node.plan_rows
        )
    };

    let bar_track = row()
        .w(88.0)
        .h(6.0)
        .radius(3.0)
        .fill(c.layer_02)
        .border(0.5, c.border_muted)
        .child(
            el().h_full()
                .w((88.0 * share).clamp(2.0, 88.0))
                .radius(3.0)
                .fill(bar_fill),
        );

    top = top
        .child(spacer())
        .child(text(right_metric).font(mono).color(if hot { c.warning.fg } else { c.text_muted }))
        .child(bar_track);

    let mut card = col()
        .key(format!("explain/node/{idx}"))
        .grow(1.0)
        .min_w(0.0)
        .px(10.0)
        .py(7.0)
        .gap(4.0)
        .radius(k.radius.sm)
        .fill(c.layer_01)
        .border(0.5, if hot { c.warning.border } else { c.border_muted })
        .child(top);

    let mut details = Vec::new();
    if let Some(cond) = &node.index_cond {
        details.push(format!("Index Cond: {cond}"));
    }
    if let Some(cond) = &node.hash_cond {
        details.push(format!("Hash Cond: {cond}"));
    }
    if let Some(filter) = &node.filter {
        details.push(format!("Filter: {filter}"));
    }
    if !node.sort_key.is_empty() {
        details.push(format!("Sort Key: {}", node.sort_key.join(", ")));
    }
    if let Some(removed) = node.rows_removed_by_filter
        && removed > 0
    {
        details.push(format!("Filtered out: {removed} rows"));
    }

    if !details.is_empty() {
        card = card.child(
            text(details.join("  ·  "))
                .font(k.ty.small.mono())
                .color(c.text_muted)
                .w_full()
                .wrap(),
        );
    }

    if depth == 0 {
        card.w_full()
    } else {
        row()
            .w_full()
            .pl((depth as f32 * 20.0).min(160.0))
            .gap(6.0)
            .items_start()
            .child(
                el().mt(10.0)
                    .size(8.0, 1.5)
                    .fill(c.border_strong),
            )
            .child(card)
    }
}

fn node_badge_colors(node_type: &str, k: &Kit) -> (rune::Color, rune::Color, rune::Color) {
    let c = &k.c;
    let upper = node_type.to_ascii_uppercase();
    if node_type == "Seq Scan"
        || upper == "SCAN"
        || upper.contains("FULL TABLE SCAN")
        || upper.contains("TABLE SCAN")
    {
        (c.warning.bg, c.warning.fg, c.warning.border)
    } else if upper == "SEARCH" || node_type.contains("Index") || upper.contains("INDEX") {
        (c.success.bg, c.success.fg, c.success.border)
    } else if node_type.contains("Join") || upper.contains("NESTED LOOP") {
        (c.info.bg, c.info.fg, c.info.border)
    } else {
        (c.layer_02, c.text_base, c.border_base)
    }
}
