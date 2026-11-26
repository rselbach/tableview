#![allow(clippy::expect_used)]

use rune::{Env, Key, KeyEvent, Mods, Runtime};
use rune_kit::diff::DiffMode;
use tv_app::scenes::{reset_to_sample_state, sample_store_data};
use tv_app::state::{AppState, OverlayState, PickerKind, RowKey, Screen, TableViewMode};
use tv_app::views;
use tv_core::{EnvTag, ExportFormat, SafetyPolicy};

fn sample_runtime(dark: bool) -> Runtime<AppState> {
    let mut state = AppState::init_in_memory(sample_store_data());
    reset_to_sample_state(&mut state, dark);
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(dark, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt
}

#[test]
fn launcher_renders_filters_and_opens_and_dismisses_connection_form() {
    let mut rt = sample_runtime(true);
    rt.frame(0.0);

    // All 4 Greendale sample connections should render in the accessibility tree.
    let tree = rt.accessibility_tree();
    let has_label = |label: &str| tree.nodes.iter().any(|(_, n)| n.label() == Some(label));
    assert!(has_label("greendale_local"));
    assert!(has_label("greendale_prod"));

    // Filter by Production in left rail.
    let (x, y) = rt.find("launcher/filter/Production").expect("Production filter");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    let filtered_tree = rt.accessibility_tree();
    assert!(filtered_tree.nodes.iter().any(|(_, n)| n.label() == Some("greendale_prod")));
    assert!(!filtered_tree.nodes.iter().any(|(_, n)| n.label() == Some("greendale_local")));

    // Open New Connection form via Cmd+N and dismiss via Escape.
    rt.key(KeyEvent {
        key: Key::Command('n'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert!(matches!(rt.state.overlay, OverlayState::ConnectionForm(_)));
    assert!(rt.find("connection-form-dialog").is_some());

    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.0);
    assert!(matches!(rt.state.overlay, OverlayState::None));
    assert!(rt.find("connection-form-dialog").is_none());
}

#[test]
fn workspace_database_picker_and_pane_toggles_work() {
    let mut rt = sample_runtime(false);
    // Apply the connected workspace scene directly.
    rt.update(|s, _| {
        let (_, apply) = tv_app::scenes::SCENES
            .iter()
            .find(|(name, _)| *name == "workspace-connected-light")
            .expect("connected light scene");
        apply(s);
    });
    rt.frame(0.0);

    assert!(matches!(rt.state.screen, Screen::Workspace(_)));
    assert!(rt.find("workspace-console").is_some());
    assert!(rt.find("inspector-pane-inner").is_some());

    // Toggle console off via Cmd+2.
    rt.key(KeyEvent {
        key: Key::Command('2'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert!(!rt.state.active_session().expect("session").console_open);
    assert!(rt.find("workspace-console").is_none());

    // Toggle inspector off via Cmd+3.
    rt.key(KeyEvent {
        key: Key::Command('3'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert!(!rt.state.active_session().expect("session").inspector_pane.is_open());

    // Toggle sidebar off via Cmd+1.
    rt.key(KeyEvent {
        key: Key::Command('1'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert!(!rt.state.active_session().expect("session").sidebar_pane.is_open());

    // Open Cmd+K database picker, type filter, and select a database.
    rt.key(KeyEvent {
        key: Key::Command('k'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert!(matches!(
        rt.state.overlay,
        OverlayState::Picker(ref p) if p.kind == PickerKind::Database
    ));
    assert!(rt.find("picker-dialog").is_some());

    // Type "study" to filter down to study_groups_dev and hit Enter.
    rt.key(KeyEvent::from(Key::Text("study".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.0);

    assert!(matches!(rt.state.overlay, OverlayState::None));
    let active_db = rt
        .state
        .active_session()
        .expect("session")
        .status
        .as_connected()
        .expect("connected")
        .active_database
        .as_deref();
    assert_eq!(active_db, Some("study_groups_dev"));
}

#[test]
fn all_scenes_render_cleanly_in_dark_and_light_modes() {
    for &(name, apply) in tv_app::scenes::SCENES {
        let mut rt = sample_runtime(true);
        rt.update(|s, _| apply(s));
        rt.frame(0.0);
        assert!(rt.find("kit-root").is_some(), "scene {name} failed to mount root");
    }
}

fn apply_scene(rt: &mut Runtime<AppState>, scene_name: &str) {
    rt.update(|s, _| {
        let (_, apply) = tv_app::scenes::SCENES
            .iter()
            .find(|(name, _)| *name == scene_name)
            .expect("registered scene");
        apply(s);
    });
    rt.frame(0.0);
}

#[test]
fn workspace_tabs_open_switch_and_close_via_cmd_w() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");

    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 2);

    // Clicking enrollments in the sidebar opens a 3rd tab without duplicating existing ones.
    let (ex, ey) = rt.find("sidebar/obj/public.enrollments").expect("enrollments item");
    rt.pointer_down(ex, ey);
    rt.pointer_up(ex, ey);
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 3);

    // Clicking enrollments again focuses it rather than opening a 4th tab.
    rt.pointer_down(ex, ey);
    rt.pointer_up(ex, ey);
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 3);

    // Cmd+W closes the active tab and returns focus to neighbor tab.
    rt.key(KeyEvent {
        key: Key::Command('w'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 2);
}

#[test]
fn workspace_data_grid_row_selection_populates_typed_inspector() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");

    // Row 0 is pre-selected in scene: row fields card should be mounted with JSONB & UTC/Local info.
    assert!(rt.find("inspector-row-fields").is_some());
    assert!(rt.find("inspector/row-badge").is_some());

    let tree = rt.accessibility_tree();
    assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some("Copy profile")));

    // Selecting row 1 (Abed Nadir) updates inspector and renders NULL badge for nullable email.
    // `r-1` spans the full horizontal scroll width, so click inside its visible left cell band.
    let r1 = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("r-1"))
        .expect("second row r-1");
    let (rx, ry) = (r1.rect[0] + 100.0, r1.rect[1] + r1.rect[3] / 2.0);
    rt.pointer_down(rx, ry);
    rt.pointer_up(rx, ry);
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .selected_row,
        Some(1)
    );
    assert!(rt.find("inspector/null/3").is_some(), "email column NULL badge rendered");

    // Clearing row selection returns inspector to table stats.
    rt.update(|s, _| s.clear_active_tab_row_selection());
    rt.frame(0.0);
    assert!(rt.find("inspector-table-stats").is_some());
}

#[test]
fn workspace_view_modes_switch_between_data_structure_and_indexes() {
    let mut rt = sample_runtime(false);
    apply_scene(&mut rt, "workspace-data-grid-light");

    assert!(rt.find("table-filter-bar").is_some());

    let (sx, sy) = rt
        .find("table-view-switch/segment1")
        .expect("Structure segment button");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .view_mode,
        TableViewMode::Structure
    );
    assert!(rt.find("structure-view-root").is_some());
    assert!(rt.find("structure-ddl-section").is_some());

    let (ix, iy) = rt
        .find("table-view-switch/segment2")
        .expect("Indexes segment button");
    rt.pointer_down(ix, iy);
    rt.pointer_up(ix, iy);
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .view_mode,
        TableViewMode::Indexes
    );
    assert!(rt.find("tv-idx-1").is_some());
}

#[test]
fn workspace_filter_bar_applies_and_clears_where_clause() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");

    let (fx, fy) = rt.find("table-filter-input").expect("filter input");
    rt.pointer_down(fx, fy);
    rt.pointer_up(fx, fy);
    rt.frame(0.0);

    rt.key(KeyEvent::from(Key::Text("enrolled = true".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.0);

    let applied = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab")
        .applied_filter
        .as_deref();
    assert_eq!(applied, Some("enrolled = true"));

    let (cx, cy) = rt.find("filter-clear-btn").expect("filter clear button");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);
    rt.frame(0.0);

    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .applied_filter,
        None
    );
}

#[test]
fn sql_workbench_opens_formats_and_saves_query() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-connected-dark");

    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 2);

    // Cmd+T opens a new SQL workbench tab and focuses its editor.
    rt.key(KeyEvent {
        key: Key::Command('t'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 3);
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .is_some()
    );
    let tab_id = rt
        .state
        .active_session()
        .expect("session")
        .active_tab
        .expect("active tab")
        .get();
    let editor_key = format!("sql-editor-{tab_id}");
    assert!(rt.find(&editor_key).is_some());

    // Typing SQL and pressing Enter inserts a newline rather than submitting away.
    rt.key(KeyEvent::from(Key::Text("select full_name, gpa".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("from public.students where gpa >= 3.5".to_owned())));
    rt.frame(0.0);
    let typed = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab")
        .editor
        .text
        .clone();
    assert!(typed.contains('\n'), "plain Enter should insert newline in SQL editor: {typed:?}");

    // Format button uppercases SQL keywords cleanly.
    rt.frame(0.5);
    let (fx, fy) = rt.find("query-format-btn").expect("format button");
    rt.pointer_down(fx, fy);
    rt.pointer_up(fx, fy);
    rt.frame(0.1);
    let formatted = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab")
        .editor
        .text
        .clone();
    assert!(formatted.contains("SELECT"));
    assert!(formatted.contains("FROM"));
    assert!(formatted.contains("WHERE"));

    // Cmd+S opens the Save Query modal; replacing name and submitting saves to library.
    rt.key(KeyEvent {
        key: Key::Command('s'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.5);
    assert!(matches!(rt.state.overlay, OverlayState::SaveQuery(_)));
    assert!(rt.find("save-query-name").is_some());

    rt.key(KeyEvent {
        key: Key::Command('a'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("High GPA Roster".to_owned())));
    rt.frame(0.0);

    let (sx, sy) = rt.find("save-query-submit-btn").expect("save submit");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);

    assert!(matches!(rt.state.overlay, OverlayState::None));
    let sess = rt.state.active_session().expect("session");
    assert!(sess.saved_queries.iter().any(|q| q.name == "High GPA Roster"));
    let active_q = sess.active_query_tab().expect("active query tab");
    assert_eq!(active_q.title, "High GPA Roster");
    assert!(!active_q.is_dirty());
}

#[test]
fn sql_workbench_switches_multi_results_and_populates_inspector() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-editor-dark");

    // Pre-selected row 0 populates the right inspector.
    assert!(rt.find("inspector-row-fields").is_some());
    assert!(rt.find("inspector/row-badge").is_some());

    // Selecting row 1 (Abed Nadir) updates inspector and shows email NULL pill.
    let r1 = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("r-1"))
        .expect("second result row r-1");
    let (rx, ry) = (r1.rect[0] + 100.0, r1.rect[1] + r1.rect[3] / 2.0);
    rt.pointer_down(rx, ry);
    rt.pointer_up(rx, ry);
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .expect("query tab")
            .active_result_state()
            .expect("res0")
            .selected_row,
        Some(1)
    );
    assert!(rt.find("inspector/null/3").is_some());

    // Switching to Result 2 updates active_result and displays active_students table.
    let (tx, ty) = rt.find("query-result-tab-1").expect("result tab 1");
    rt.pointer_down(tx, ty);
    rt.pointer_up(tx, ty);
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .expect("query tab")
            .active_result,
        1
    );
    assert!(rt.find("r-0").is_some());
}

#[test]
fn sidebar_queries_and_history_open_without_duplicating_saved_tabs() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-saved-queries");

    let first_saved = rt
        .state
        .active_session()
        .expect("session")
        .saved_queries[0]
        .clone();
    let initial_tabs = rt.state.active_session().expect("session").tabs.len();
    assert_eq!(initial_tabs, 3);

    // Click first saved query in sidebar -> opens tab 4.
    let key = format!("sidebar/saved/{}", first_saved.id);
    let (qx, qy) = rt.find(&key).expect("saved query sidebar item");
    rt.pointer_down(qx, qy);
    rt.pointer_up(qx, qy);
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 4);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .expect("query tab")
            .title,
        first_saved.name
    );

    // Clicking the same saved query again focuses it without duplicating.
    let (qx2, qy2) = rt.find(&key).expect("saved query sidebar item");
    rt.pointer_down(qx2, qy2);
    rt.pointer_up(qx2, qy2);
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 4);

    // Switch sidebar to History and open history entry 1.
    let hist_node = rt
        .accessibility_tree()
        .nodes
        .into_iter()
        .find(|(_, n)| n.label() == Some("History"))
        .map(|(id, _)| id)
        .expect("History sidebar tab");
    let _ = hist_node;
    let hist_tab = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref().is_some_and(|k| k.ends_with("segment2")))
        .expect("sidebar History segment");
    rt.pointer_down(
        hist_tab.rect[0] + hist_tab.rect[2] / 2.0,
        hist_tab.rect[1] + hist_tab.rect[3] / 2.0,
    );
    rt.pointer_up(
        hist_tab.rect[0] + hist_tab.rect[2] / 2.0,
        hist_tab.rect[1] + hist_tab.rect[3] / 2.0,
    );
    rt.frame(0.5);

    let (hx, hy) = rt.find("sidebar/history/1").expect("history entry 1");
    rt.pointer_down(hx, hy);
    rt.pointer_up(hx, hy);
    rt.frame(0.1);
    assert_eq!(rt.state.active_session().expect("session").tabs.len(), 5);
}

#[test]
fn milestone4_inline_editing_insert_delete_and_staged_diff_drawer() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    rt.frame(0.5);

    // Click "+ Row" -> inserts draft row and focuses inline-cell-editor.
    let (ix, iy) = rt.find("table-insert-row-btn").expect("insert row btn");
    rt.pointer_down(ix, iy);
    rt.pointer_up(ix, iy);
    rt.frame(0.5);
    assert!(rt.find("inline-cell-editor").is_some());

    // Clear auto-selected text by typing and submit via Enter.
    rt.key(KeyEvent::from(Key::Text("Señor Chang".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    let tab = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert_eq!(tab.staged.insert_count(), 1);

    // Programmatically begin edit on existing row 0 full_name (col 2), enter new value & commit.
    rt.update(|s, tx| {
        s.begin_cell_edit(RowKey::Existing(0), 2, tx);
    });
    rt.frame(0.2);
    rt.key(KeyEvent::from(Key::Text("Troy Updated".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    // Mark Jeff Winger (existing row 3) for deletion.
    rt.update(|s, _| {
        s.toggle_delete_row(RowKey::Existing(3));
    });
    rt.frame(0.1);

    let tab = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert_eq!(tab.staged.update_count(), 1);
    assert_eq!(tab.staged.delete_count(), 1);
    assert!(rt.find("table-staged-banner").is_some());

    // Press Cmd+S on a table tab with staged changes -> opens staged diff drawer.
    rt.key(KeyEvent {
        key: Key::Command('s'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.5);
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .diff_drawer_open
    );
    assert!(rt.find("staged-diff-drawer").is_some());

    // Switch Unified -> Split diff mode.
    let (sx, sy) = rt
        .find("staged-diff-mode-switch/segment1")
        .expect("Split segment");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .diff_state
            .mode(),
        DiffMode::Split
    );

    // Discard staged changes via drawer button.
    let (dx, dy) = rt.find("staged-discard-btn").expect("discard btn");
    rt.pointer_down(dx, dy);
    rt.pointer_up(dx, dy);
    rt.frame(0.1);
    let tab_after = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert!(!tab_after.diff_drawer_open);
    assert!(tab_after.staged.is_empty());
}

#[test]
fn milestone4_safety_policies_block_and_gate_production_destructive_writes() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-editor-dark");
    rt.frame(0.5);

    // Set ReadOnly and replace SQL with a DELETE statement -> blocked immediately without modal.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        sess.profile.safety = SafetyPolicy::ReadOnly;
        let q = sess.active_query_tab_mut().expect("query tab");
        q.editor = rune::TextEdit::new("DELETE FROM public.students WHERE student_id = 4;");
        s.run_active_query(true);
    });
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));
    let err = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab")
        .batch_error
        .as_deref()
        .expect("blocked error");
    assert!(err.contains("Read-Only"), "unexpected block msg: {err}");

    // Production + ConfirmWrites on DELETE without WHERE -> opens SafetyConfirm with destructive gate.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        sess.profile.env = EnvTag::Production;
        sess.profile.safety = SafetyPolicy::ConfirmWrites;
        let q = sess.active_query_tab_mut().expect("query tab");
        q.batch_error = None;
        q.editor = rune::TextEdit::new("DELETE FROM public.students;");
        s.run_active_query(true);
    });
    rt.frame(0.5);
    assert!(matches!(
        rt.state.overlay,
        OverlayState::SafetyConfirm(ref m) if m.is_destructive && !m.destructive_ack
    ));
    assert!(rt.find("safety-confirm-modal-dialog").is_some());
    assert!(rt.find("safety-prod-ack").is_some());

    // Toggle Production destructive acknowledgement checkbox and cancel cleanly.
    let (ax, ay) = rt.find("safety-prod-ack").expect("prod ack checkbox");
    rt.pointer_down(ax, ay);
    rt.pointer_up(ax, ay);
    rt.frame(0.1);
    assert!(matches!(
        rt.state.overlay,
        OverlayState::SafetyConfirm(ref m) if m.destructive_ack
    ));

    let (cx, cy) = rt.find("safety-cancel-btn").expect("safety cancel");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));
}

#[test]
fn milestone4_fk_peek_opens_and_jumps_to_referenced_table_tab() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-fk-peek");
    rt.frame(0.5);

    assert!(rt.find("fk-peek-card").is_some());
    assert!(rt.find("fk-peek-open-tab-btn").is_some());

    let (jx, jy) = rt.find("fk-peek-open-tab-btn").expect("Open in Tab button");
    rt.pointer_down(jx, jy);
    rt.pointer_up(jx, jy);
    rt.frame(0.2);

    let tab = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("jumped table tab");
    assert_eq!(tab.schema, "public");
    assert_eq!(tab.object_name, "study_groups");
    assert_eq!(tab.applied_filter.as_deref(), Some("\"group_id\" = 1"));
    assert!(tab.fk_peek.is_none());
}

#[test]
fn milestone4_visual_explain_renders_and_toggles_json() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-explain-plan");
    rt.frame(0.5);

    assert!(rt.find("explain-pane-root").is_some());
    assert!(rt.find("explain/node/0").is_some());
    assert!(rt.find("explain-json-scroll").is_none());

    let (jx, jy) = rt.find("explain-mode-switch/segment1").expect("JSON segment");
    rt.pointer_down(jx, jy);
    rt.pointer_up(jx, jy);
    rt.frame(0.1);

    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .expect("query tab")
            .explain_raw_json
    );
    assert!(rt.find("explain-json-scroll").is_some());
}

#[test]
fn milestone4_export_modal_switches_formats_and_copies() {
    let mut rt = sample_runtime(false);
    apply_scene(&mut rt, "workspace-export-modal");
    rt.frame(0.5);

    assert!(rt.find("export-modal-dialog").is_some());
    assert!(rt.find("export-table-name").is_some());

    // Switch to JSON then back to CSV, then SQL INSERT and Copy.
    let (jx, jy) = rt.find("export-format-switch/segment1").expect("JSON segment");
    rt.pointer_down(jx, jy);
    rt.pointer_up(jx, jy);
    rt.frame(0.1);
    assert!(matches!(
        rt.state.overlay,
        OverlayState::Export(ref m) if m.format == ExportFormat::Json
    ));

    let (sx, sy) = rt.find("export-format-switch/segment2").expect("SQL INSERT segment");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);
    assert!(matches!(
        rt.state.overlay,
        OverlayState::Export(ref m) if m.format == ExportFormat::SqlInsert
    ));

    let (cx, cy) = rt.find("export-copy-btn").expect("Copy to Clipboard button");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|(msg, is_err)| !is_err && msg.contains("Copied 8 rows as SQL INSERT"))
    );
}
