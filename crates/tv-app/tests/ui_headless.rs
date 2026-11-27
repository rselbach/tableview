#![allow(clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::{ColorScheme, Env, Key, KeyEvent, Mods, Runtime};
use rune_kit::diff::DiffMode;
use tv_app::platform::{Action, dispatch_action};
use tv_app::scenes::{reset_to_sample_state, sample_store_data};
use tv_app::state::{AppState, ConnectionState, OverlayState, PickerKind, RowKey, Screen, TableViewMode};
use tv_app::views;
use tv_app::worker::{SessionConnector, WorkerBridge};
use tv_core::{
    AppearancePreference, CellValue, ColumnMeta, ColumnSummary, ConnectionProfile, DatabaseInfo, DbEngine, DbError,
    DbObject, DbObjectKind, DbSession, EnvTag, ExecutedSql, ExplainPlan, ExportFormat, FkPeekRow, ForeignKeyRef,
    IndexInfo, PageRequest, PasswordMode, QueryBatchResult, QueryStatementResult, SafetyPolicy, SchemaInfo,
    ServerInfo, SettingsStore, StoreData, TablePage, TableStats, TableStructure, TransactionOutcome,
};
use tv_update::{UpdateFailure, UpdateStatus};

fn sample_runtime(dark: bool) -> Runtime<AppState> {
    let mut state = AppState::init_in_memory(sample_store_data());
    reset_to_sample_state(&mut state, dark);
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(dark, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
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
fn inline_cell_editor_closes_on_outside_click() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    rt.frame(0.5);

    let begin_email_edit = |rt: &mut Runtime<AppState>| {
        rt.update(|s, tx| {
            assert!(s.begin_cell_edit(RowKey::Existing(0), 3, tx));
        });
        rt.frame(0.2);
        assert!(rt.find("inline-cell-editor").is_some());
        assert!(rt.find("inline-null-btn").is_some());
    };

    // 1. Clicking inside the inline text input keeps the editor open.
    begin_email_edit(&mut rt);
    let (ex, ey) = rt.find("inline-cell-editor").expect("inline cell editor");
    rt.pointer_down(ex, ey);
    rt.pointer_up(ex, ey);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_some());
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .editing_cell
            .is_some()
    );

    // 2. Clicking empty space below rows closes the editor without staging an edit.
    let data_view = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("data-view-root"))
        .expect("data-view-root");
    let empty_x = data_view.rect[0] + data_view.rect[2] * 0.5;
    let empty_y = data_view.rect[1] + data_view.rect[3] - 24.0;
    rt.pointer_down(empty_x, empty_y);
    rt.pointer_up(empty_x, empty_y);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_none());
    let tab = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert!(tab.editing_cell.is_none());
    assert!(tab.staged.is_empty());

    // 3. Clicking an inspector edit button closes col 3 and switches directly to col 2.
    begin_email_edit(&mut rt);
    let (ix, iy) = rt.find("inspector/edit/2").expect("inspector edit full_name");
    rt.pointer_down(ix, iy);
    rt.pointer_up(ix, iy);
    rt.frame(0.2);
    assert!(rt.find("inline-cell-editor").is_some());
    let editing = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab")
        .editing_cell
        .as_ref()
        .expect("editing switched to col 2");
    assert_eq!(editing.col_idx, 2);

    // 4. Clicking the WHERE filter bar closes the editor and gives focus to the filter input.
    let (fx, fy) = rt.find("table-filter-input").expect("table filter input");
    rt.pointer_down(fx, fy);
    rt.pointer_up(fx, fy);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_none());
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .editing_cell
            .is_none()
    );
    rt.key(KeyEvent::from(Key::Text("gpa > 3.0".to_owned())));
    rt.frame(0.0);
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .filter_input
            .text,
        "gpa > 3.0"
    );

    // 5. Clicking in the sidebar closes the editor.
    begin_email_edit(&mut rt);
    let (sx, sy) = rt
        .find("sidebar/obj/public.students/toggle")
        .expect("students sidebar toggle");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_none());
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .editing_cell
            .is_none()
    );

    // 6. With the floating overlay active over `email`, clicking the unobstructed NULL button stages NULL.
    begin_email_edit(&mut rt);
    let (nx, ny) = rt.find("inline-null-btn").expect("inline NULL button");
    rt.pointer_down(nx, ny);
    rt.pointer_up(nx, ny);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_none());
    let tab_after = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert!(tab_after.editing_cell.is_none());
    assert_eq!(tab_after.staged.update_count(), 1);

    // 7. Clicking an open inspector area outside `inline-editor-wrap` closes the editor without staging.
    begin_email_edit(&mut rt);
    let inspector = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("inspector-pane-inner"))
        .expect("inspector-pane-inner");
    let insp_x = inspector.rect[0] + inspector.rect[2] * 0.5;
    let insp_y = inspector.rect[1] + inspector.rect[3] - 24.0;
    rt.pointer_down(insp_x, insp_y);
    rt.pointer_up(insp_x, insp_y);
    rt.frame(0.1);
    assert!(rt.find("inline-cell-editor").is_none());
    let tab_insp = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert!(tab_insp.editing_cell.is_none());
    assert_eq!(tab_insp.staged.update_count(), 1);
}

#[test]
fn inline_cell_editor_expands_for_long_and_multiline_values() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    rt.frame(0.5);

    let elem_rect = |rt: &Runtime<AppState>, key: &str| {
        rt.elements()
            .into_iter()
            .find(|el| el.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("missing element {key}"))
            .rect
    };

    // 1. Editing a 36-char UUID expands horizontally past the column width on a single line.
    rt.update(|s, tx| {
        assert!(s.begin_cell_edit(RowKey::Existing(0), 1, tx));
    });
    rt.frame(0.2);

    let anchor_rect = elem_rect(&rt, "inline-editor-anchor");
    let wrap_rect = elem_rect(&rt, "inline-editor-wrap");
    let editor_rect = elem_rect(&rt, "inline-cell-editor");
    assert!(
        wrap_rect[2] > anchor_rect[2],
        "floating editor width {} should exceed cell anchor width {}",
        wrap_rect[2],
        anchor_rect[2]
    );
    assert!(
        wrap_rect[3] <= 34.0,
        "UUID editor should stay single-line, got height {}",
        wrap_rect[3]
    );
    assert!(rt.find("inline-editor-footer").is_none());
    assert!(
        editor_rect[1] + editor_rect[3] <= wrap_rect[1] + wrap_rect[3] + 0.5,
        "editor text {:?} overflowed wrap {:?}",
        editor_rect,
        wrap_rect
    );

    // 2. Escape cancels the floating editor cleanly.
    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.1);
    assert!(rt.find("inline-editor-wrap").is_none());
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab")
            .editing_cell
            .is_none()
    );

    // 3. Typing a long >70-char string dynamically expands into multi-line mode without losing focus.
    rt.update(|s, tx| {
        assert!(s.begin_cell_edit(RowKey::Existing(0), 2, tx));
    });
    rt.frame(0.2);
    rt.key(KeyEvent::from(Key::Text(
        "Troy Barnes and Abed Nadir in the Morning at Greendale Community College Study Room F".to_owned(),
    )));
    rt.frame(0.1);

    let multi_wrap = elem_rect(&rt, "inline-editor-wrap");
    let multi_editor = elem_rect(&rt, "inline-cell-editor");
    assert!(multi_wrap[2] >= 340.0, "expected >= 340 width, got {}", multi_wrap[2]);
    assert!(multi_wrap[3] > 48.0, "expected multi-line height > 48, got {}", multi_wrap[3]);
    assert!(rt.find("inline-editor-footer").is_some());
    assert!(
        multi_editor[1] + multi_editor[3] <= multi_wrap[1] + multi_wrap[3] + 0.5,
        "multi-line editor {:?} overflowed wrap {:?}",
        multi_editor,
        multi_wrap
    );
    assert!(multi_wrap[0] >= 8.0);
    assert!(multi_wrap[0] + multi_wrap[2] <= 1272.5);
    assert!(multi_wrap[1] + multi_wrap[3] <= 792.5);

    // 4. Shift+Enter inserts a newline while keeping focus; Enter commits the staged edit.
    rt.key(KeyEvent {
        key: Key::Enter,
        mods: Mods {
            shift: true,
            ..Default::default()
        },
    });
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("Second line".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    let tab = rt
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert!(tab.editing_cell.is_none());
    assert_eq!(tab.staged.update_count(), 1);
    match tab.staged.effective_existing_cell(0, 2) {
        Some(tv_core::StagedCellValue::Literal(val)) => {
            assert!(
                val.contains("\nSecond line"),
                "expected Shift+Enter newline in staged value, got {val:?}"
            );
        }
        other => panic!("expected staged literal update, got {other:?}"),
    }
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
            .is_some_and(|n| !n.is_error && n.message.contains("Copied 8 rows as SQL INSERT"))
    );
}

fn has_text(rt: &Runtime<AppState>, expected: &str) -> bool {
    rt.elements()
        .iter()
        .any(|e| e.text.as_deref() == Some(expected))
}

#[test]
fn update_card_hidden_when_no_update_attention_needed() {
    let mut rt = sample_runtime(true);
    rt.frame(0.0);
    assert!(rt.find("update-dock-card").is_none());

    for status in [
        UpdateStatus::Idle,
        UpdateStatus::Checking,
        UpdateStatus::UpToDate,
        UpdateStatus::Failed(UpdateFailure::Check),
        UpdateStatus::Failed(UpdateFailure::Verify),
    ] {
        rt.update(|s, _| s.update.snapshot.status = status);
        rt.frame(0.0);
        assert!(rt.find("update-dock-card").is_none());
    }

    apply_scene(&mut rt, "workspace-connected-light");
    rt.frame(0.0);
    assert!(rt.find("update-dock-card").is_none());
    assert!(rt.find("sidebar-toggle-unread").is_none());
}

#[test]
fn launcher_update_ready_renders_and_toggles_notes() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "launcher-update-ready");
    rt.frame(0.1);

    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("update-card-restart-btn").is_some());
    assert!(rt.find("update-card-notes").is_none());
    assert!(has_text(&rt, "v0.2.0 Ready"));
    assert!(has_text(&rt, "Restart TableView to update"));

    let (nx, ny) = rt.find("update-card-notes-btn").expect("Notes toggle button");
    rt.pointer_down(nx, ny);
    rt.pointer_up(nx, ny);
    rt.frame(0.1);

    assert!(rt.find("update-card-notes").is_some());
    assert!(rt.find("update-card-open-notes-btn").is_some());
    assert!(rt.state.update.notes_expanded);

    let (hx, hy) = rt.find("update-card-notes-btn").expect("Hide notes toggle button");
    rt.pointer_down(hx, hy);
    rt.pointer_up(hx, hy);
    rt.frame(0.1);

    assert!(rt.find("update-card-notes").is_none());
    assert!(!rt.state.update.notes_expanded);

    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::CheckForUpdates, tx));
    });
    let notice = rt.state.status_notice.as_ref().expect("status notice");
    assert_eq!(notice.message, "TableView 0.2.0 is ready to install on restart.");
    assert!(!notice.is_error);
}

#[test]
fn workspace_update_ready_persists_across_tabs_collapse_dot_and_staged_warning() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-update-ready");
    rt.frame(0.2);

    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("sidebar-toggle-unread").is_some());

    // Switch sidebar to Queries (segment1) and History (segment2); card stays docked.
    for segment_suffix in ["segment1", "segment2"] {
        let seg = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref().is_some_and(|k| k.ends_with(segment_suffix)))
            .expect("sidebar tab segment");
        rt.pointer_down(seg.rect[0] + seg.rect[2] / 2.0, seg.rect[1] + seg.rect[3] / 2.0);
        rt.pointer_up(seg.rect[0] + seg.rect[2] / 2.0, seg.rect[1] + seg.rect[3] / 2.0);
        rt.frame(0.1);
        assert!(rt.find("update-dock-card").is_some());
    }

    // Collapse sidebar via Cmd+1 -> toggle keeps attention dot while sidebar closes.
    rt.key(KeyEvent {
        key: Key::Command('1'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.2);
    assert!(!rt.state.active_session().expect("session").sidebar_pane.is_open());
    assert!(rt.find("sidebar-toggle-unread").is_some());

    // Reopen sidebar via Cmd+1.
    rt.key(KeyEvent {
        key: Key::Command('1'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.2);
    assert!(rt.state.active_session().expect("session").sidebar_pane.is_open());
    assert!(rt.find("update-dock-card").is_some());

    // Stage an uncommitted row insert and confirm the staged-edits notice renders in the card.
    let (ix, iy) = rt.find("table-insert-row-btn").expect("insert row btn");
    rt.pointer_down(ix, iy);
    rt.pointer_up(ix, iy);
    rt.frame(0.2);
    rt.key(KeyEvent::from(Key::Text("Britta Perry".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    assert!(rt.state.has_uncommitted_staged_edits());
    assert!(has_text(&rt, "Uncommitted staged edits stay local until committed."));
}

#[test]
fn update_card_renders_available_downloading_relaunch_and_os_warning_states() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "launcher-dark");
    rt.frame(0.1);

    // Available fallback (e.g. read-only mount) shows manual Download CTA and Notes toggle.
    rt.update(|s, _| {
        s.update.snapshot.status = UpdateStatus::Available {
            version: "0.3.0".to_owned(),
            download_url: "https://github.com/rselbach/tableview/releases/download/v0.3.0/TableView-0.3.0-macos-arm64.dmg".to_owned(),
            notes_url: Some("https://github.com/rselbach/tableview/releases/tag/v0.3.0".to_owned()),
            highlights: vec!["Adds signed update feed support".to_owned()],
        };
    });
    rt.frame(0.1);
    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("update-card-download-btn").is_some());
    let (nx, ny) = rt.find("update-card-notes-btn").expect("notes btn");
    rt.pointer_down(nx, ny);
    rt.pointer_up(nx, ny);
    rt.frame(0.1);
    assert!(rt.find("update-card-notes").is_some());

    // Downloading scene shows progress bar and percentage.
    apply_scene(&mut rt, "workspace-update-downloading");
    rt.frame(0.2);
    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("update-card-progress-track").is_some());
    assert!(rt.find("sidebar-toggle-unread").is_some());
    assert!(has_text(&rt, "Downloading v0.2.0"));
    assert!(has_text(&rt, "64%"));

    // Relaunch failure after swap tells the user to reopen manually and lights the collapsed dot.
    rt.update(|s, _| {
        s.update.snapshot.status = UpdateStatus::Failed(UpdateFailure::Relaunch {
            version: "0.2.0".to_owned(),
        });
    });
    rt.frame(0.1);
    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("sidebar-toggle-unread").is_some());
    assert!(has_text(&rt, "v0.2.0 Installed"));
    assert!(has_text(&rt, "Quit and reopen TableView to finish updating."));

    // RequiresNewerOs warning is shown and can be dismissed.
    rt.update(|s, _| {
        s.update.snapshot.status = UpdateStatus::Failed(UpdateFailure::RequiresNewerOs {
            version: "0.9.0".to_owned(),
            minimum_os: "16.0".to_owned(),
        });
    });
    rt.frame(0.1);
    assert!(rt.find("update-dock-card").is_some());
    assert!(has_text(&rt, "v0.9.0 needs macOS 16.0+"));
    let (dx, dy) = rt.find("update-card-dismiss-btn").expect("dismiss button");
    rt.pointer_down(dx, dy);
    rt.pointer_up(dx, dy);
    rt.frame(0.1);
    assert!(rt.state.update.dismissed_os_warning);
    assert!(rt.find("update-dock-card").is_none());
}

#[test]
fn launcher_omits_bottom_left_appearance_and_opens_settings_dialog() {
    let mut rt = sample_runtime(true);
    rt.frame(0.0);

    assert!(rt.find("launcher-theme-wrap").is_none());
    assert!(!has_text(&rt, "APPEARANCE"));

    let (sx, sy) = rt.find("launcher-settings-btn").expect("launcher settings button");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);

    assert!(matches!(rt.state.overlay, OverlayState::Settings));
    assert!(rt.find("settings-modal-dialog").is_some());
    assert!(has_text(&rt, "APPEARANCE"));
    assert!(has_text(&rt, "UPDATES"));

    let (lx, ly) = rt.find("settings-appearance/segment1").expect("Light theme segment");
    rt.pointer_down(lx, ly);
    rt.pointer_up(lx, ly);
    rt.frame(0.1);
    assert_eq!(rt.state.appearance, ColorScheme::Light);

    let (dx, dy) = rt.find("settings-appearance/segment2").expect("Dark theme segment");
    rt.pointer_down(dx, dy);
    rt.pointer_up(dx, dy);
    rt.frame(0.1);
    assert_eq!(rt.state.appearance, ColorScheme::Dark);

    let (sys_x, sys_y) = rt.find("settings-appearance/segment0").expect("System theme segment");
    rt.pointer_down(sys_x, sys_y);
    rt.pointer_up(sys_x, sys_y);
    rt.frame(0.1);
    assert_eq!(rt.state.appearance, ColorScheme::System);

    let (done_x, done_y) = rt.find("settings-done-btn").expect("settings done button");
    rt.pointer_down(done_x, done_y);
    rt.pointer_up(done_x, done_y);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));
    assert!(rt.find("settings-modal-dialog").is_none());
}

#[test]
fn settings_shortcut_and_workspace_button_open_and_escape_dismisses() {
    let mut rt = sample_runtime(false);
    rt.frame(0.0);

    let open_settings_key = KeyEvent {
        key: Key::Command(','),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    };
    rt.key(open_settings_key.clone());
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::Settings));
    assert!(rt.find("settings-modal-dialog").is_some());

    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.5);
    assert!(matches!(rt.state.overlay, OverlayState::None));

    apply_scene(&mut rt, "workspace-connected-dark");
    rt.frame(0.5);
    let (wx, wy) = rt.find("workspace-settings-btn").expect("workspace settings button");
    rt.pointer_down(wx, wy);
    rt.pointer_up(wx, wy);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::Settings));
    assert!(rt.find("settings-modal-dialog").is_some());

    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.5);
    assert!(matches!(rt.state.overlay, OverlayState::None));

    rt.key(open_settings_key);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::Settings));
}

#[test]
fn settings_dialog_updates_controls_and_persists_appearance() {
    let mut rt = sample_runtime(true);
    rt.update(|s, _| s.overlay = OverlayState::Settings);
    rt.frame(0.1);

    // Disabled updater keeps the automatic-updates switch disabled.
    assert!(!rt.state.update.snapshot.automatic);
    let (sw_x, sw_y) = rt
        .find("settings-auto-updates-switch")
        .expect("automatic updates switch");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(!rt.state.update.snapshot.automatic);

    // Active updater lets the user toggle automatic checks and trigger a manual check.
    rt.update(|s, _| {
        s.update.snapshot.status = UpdateStatus::UpToDate;
        s.update.snapshot.automatic = true;
    });
    rt.frame(0.1);
    let (sw_x, sw_y) = rt
        .find("settings-auto-updates-switch")
        .expect("automatic updates switch");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(!rt.state.update.snapshot.automatic);

    let (act_x, act_y) = rt
        .find("settings-update-action-btn")
        .expect("update action button");
    rt.pointer_down(act_x, act_y);
    rt.pointer_up(act_x, act_y);
    rt.frame(0.1);
    assert!(rt.state.status_notice.is_some());

    // Appearance changes persist to settings.json when persistence is enabled.
    let dir = tempfile::tempdir().expect("tempdir");
    let settings_path = dir.path().join("settings.json");
    rt.update(|s, tx| {
        s.settings_path = settings_path.clone();
        s.persist_enabled = true;
        assert!(dispatch_action(s, Action::SetColorScheme(ColorScheme::Light), tx));
    });
    let reloaded = SettingsStore::at(settings_path.clone())
        .load()
        .expect("load persisted settings");
    assert_eq!(reloaded.appearance, AppearancePreference::Light);

    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::SetColorScheme(ColorScheme::Dark), tx));
    });
    let reloaded = SettingsStore::at(settings_path)
        .load()
        .expect("load updated settings");
    assert_eq!(reloaded.appearance, AppearancePreference::Dark);
}

#[test]
fn status_notice_auto_dismisses_unless_hovered_or_clicked() {
    let mut rt = sample_runtime(false);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    rt.frame(0.1);

    // 1. Standard status notice stays visible before 4s, then auto-dismisses past 4s.
    rt.update(|s, _| {
        s.set_notice("Committed 1 change(s) (1 rows affected · 4 ms)", false);
    });
    rt.frame(0.1);
    assert!(rt.state.status_notice.is_some());
    assert!(rt.find("tv-notice").is_some());

    rt.frame(3.0);
    assert!(rt.state.status_notice.is_some(), "notice must remain before 4s");

    rt.frame(1.5);
    assert!(rt.state.status_notice.is_none(), "notice must auto-dismiss after 4s");
    rt.frame(0.5);
    assert!(rt.find("tv-notice").is_none());

    // 2. Error notice stays visible longer (~6s) before auto-dismissing.
    rt.update(|s, _| {
        s.set_notice("Failed to commit staged changes", true);
    });
    rt.frame(0.1);
    rt.frame(4.5);
    assert!(rt.state.status_notice.is_some(), "error notice must remain at 4.6s");
    rt.frame(2.0);
    assert!(rt.state.status_notice.is_none(), "error notice must auto-dismiss after 6s");
    rt.frame(0.5);

    // 3. Hovering pauses the timer; leaving resumes with remaining/grace duration.
    rt.update(|s, _| {
        s.set_notice("Copied row as JSON", false);
    });
    rt.frame(0.1);
    let (tx, ty) = rt.find("tv-notice").expect("tv-notice");
    rt.pointer_move(tx, ty);
    rt.frame(0.1);
    rt.frame(5.0);
    rt.frame(5.0);
    assert!(rt.state.status_notice.is_some(), "hovered notice must not auto-dismiss");
    assert!(rt.find("tv-notice").is_some());

    rt.pointer_move(10.0, 10.0);
    rt.frame(0.1);
    rt.frame(4.5);
    rt.frame(0.5);
    assert!(rt.state.status_notice.is_none(), "notice must auto-dismiss after pointer leaves");

    // 4. Clicking the toast body pins it open even after moving the pointer away until closed.
    rt.update(|s, _| {
        s.set_notice("Copied DDL to clipboard", false);
    });
    rt.frame(0.1);
    let toast_rect = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("tv-notice"))
        .expect("tv-notice")
        .rect;
    let body_x = toast_rect[0] + 40.0;
    let body_y = toast_rect[1] + toast_rect[3] * 0.5;
    rt.pointer_down(body_x, body_y);
    rt.pointer_up(body_x, body_y);
    rt.frame(0.1);
    rt.pointer_move(10.0, 10.0);
    rt.frame(0.1);
    rt.frame(10.0);
    rt.frame(10.0);
    assert!(rt.state.status_notice.is_some(), "clicked notice must remain pinned");
    assert!(rt.find("tv-notice").is_some());

    let (cx, cy) = rt.find("tv-notice/top/close").expect("tv-notice close button");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);
    rt.frame(0.5);
    assert!(rt.state.status_notice.is_none());
    assert!(rt.find("tv-notice").is_none());

    // 5. Replacing a pinned notice starts a fresh auto-dismiss timer.
    rt.update(|s, _| {
        s.set_notice("Pinned notice", false);
    });
    rt.frame(0.1);
    let toast_rect = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("tv-notice"))
        .expect("tv-notice")
        .rect;
    rt.pointer_down(toast_rect[0] + 40.0, toast_rect[1] + toast_rect[3] * 0.5);
    rt.pointer_up(toast_rect[0] + 40.0, toast_rect[1] + toast_rect[3] * 0.5);
    rt.frame(0.1);
    rt.pointer_move(10.0, 10.0);
    rt.frame(0.1);

    rt.update(|s, _| {
        s.set_notice("Replacement notice", false);
    });
    rt.frame(0.1);
    rt.frame(4.5);
    assert!(rt.state.status_notice.is_none(), "replacement notice must auto-dismiss");
}

#[test]
fn connection_form_switches_engines_updates_defaults_and_swaps_sqlite_ui() {
    let mut rt = sample_runtime(true);
    rt.frame(0.0);

    // Open New Connection form via Cmd+N.
    rt.key(KeyEvent {
        key: Key::Command('n'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    assert!(rt.find("form-engine").is_some());
    assert!(has_text(&rt, "New PostgreSQL Connection"));
    assert!(rt.find("form-sqlite-path").is_none());

    // Switch to MySQL (segment1) -> default port becomes 3306 and default user becomes root.
    let (mx, my) = rt.find("form-engine/segment1").expect("MySQL engine segment");
    rt.pointer_down(mx, my);
    rt.pointer_up(mx, my);
    rt.frame(0.1);
    assert!(has_text(&rt, "New MySQL Connection"));
    match &rt.state.overlay {
        OverlayState::ConnectionForm(form) => {
            assert_eq!(form.engine, DbEngine::Mysql);
            assert_eq!(form.port.text, "3306");
            assert_eq!(form.user.text, "root");
        }
        other => panic!("expected ConnectionForm, got {other:?}"),
    }

    // Switch to SQLite (segment2) -> shows file path input + Browse button and hides network fields.
    let (sx, sy) = rt.find("form-engine/segment2").expect("SQLite engine segment");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    rt.frame(0.1);
    assert!(has_text(&rt, "New SQLite Connection"));
    assert!(rt.find("form-sqlite-path").is_some());
    assert!(rt.find("form-sqlite-browse").is_some());
    assert!(rt.find("form-ssl-mode").is_none());

    // Switch back to PostgreSQL (segment0) -> default port/user restored to 5432/postgres.
    let (px, py) = rt.find("form-engine/segment0").expect("PostgreSQL engine segment");
    rt.pointer_down(px, py);
    rt.pointer_up(px, py);
    rt.frame(0.1);
    assert!(has_text(&rt, "New PostgreSQL Connection"));
    assert!(rt.find("form-sqlite-path").is_none());
    match &rt.state.overlay {
        OverlayState::ConnectionForm(form) => {
            assert_eq!(form.engine, DbEngine::Postgres);
            assert_eq!(form.port.text, "5432");
            assert_eq!(form.user.text, "postgres");
        }
        other => panic!("expected ConnectionForm, got {other:?}"),
    }
}

#[test]
fn import_url_modal_supports_mysql_and_sqlite_urls() {
    let mut rt = sample_runtime(true);
    rt.frame(0.0);

    // 1. Import mysql:// URL.
    rt.key(KeyEvent {
        key: Key::Command('i'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    assert!(has_text(&rt, "Import Connection URL"));
    assert!(rt.find("import-url-input").is_some());

    rt.key(KeyEvent::from(Key::Text(
        "mysql://abed:secret@study-room.greendale.edu:3307/study_db".to_owned(),
    )));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    match &rt.state.overlay {
        OverlayState::ConnectionForm(form) => {
            assert_eq!(form.engine, DbEngine::Mysql);
            assert_eq!(form.host.text, "study-room.greendale.edu");
            assert_eq!(form.port.text, "3307");
            assert_eq!(form.user.text, "abed");
            assert_eq!(form.database.text, "study_db");
            assert_eq!(form.password.text, "secret");
        }
        other => panic!("expected ConnectionForm after MySQL import, got {other:?}"),
    }
    assert!(has_text(&rt, "New MySQL Connection"));

    // 2. Dismiss and import sqlite:// URL.
    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.1);
    rt.key(KeyEvent {
        key: Key::Command('i'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("sqlite:///tmp/greendale_notes.db".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    match &rt.state.overlay {
        OverlayState::ConnectionForm(form) => {
            assert_eq!(form.engine, DbEngine::Sqlite);
            assert_eq!(form.name.text, "greendale_notes");
            assert_eq!(form.file_path.text, "/tmp/greendale_notes.db");
        }
        other => panic!("expected ConnectionForm after SQLite import, got {other:?}"),
    }
    assert!(has_text(&rt, "New SQLite Connection"));
    assert!(rt.find("form-sqlite-path").is_some());
}

#[test]
fn launcher_displays_engine_badges_and_sqlite_subtitles_for_mixed_connections() {
    let mut rt = sample_runtime(true);
    let mysql_prof = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy_barnes");
    let sqlite_prof = ConnectionProfile::new_sqlite("greendale_sqlite", "/tmp/greendale.sqlite3");
    let mysql_id = mysql_prof.id;
    let sqlite_id = sqlite_prof.id;
    rt.update(|s, _| {
        s.connections.push(mysql_prof);
        s.connections.push(sqlite_prof);
    });
    rt.frame(0.1);

    assert!(has_text(&rt, "PG"));
    assert!(has_text(&rt, "MY"));
    assert!(has_text(&rt, "SL"));
    assert!(rt.find(&format!("launcher/conn/{mysql_id}/emblem")).is_some());
    assert!(rt.find(&format!("launcher/conn/{sqlite_id}/emblem")).is_some());
    assert!(has_text(&rt, "/tmp/greendale.sqlite3"));

    // Search matches SQLite file path and MySQL engine label.
    rt.update(|s, _| s.launcher_search = rune::TextEdit::new("greendale.sqlite3"));
    rt.frame(0.0);
    assert_eq!(rt.state.filtered_connections().len(), 1);
    assert_eq!(rt.state.filtered_connections()[0].id, sqlite_id);

    rt.update(|s, _| s.launcher_search = rune::TextEdit::new("mysql"));
    rt.frame(0.0);
    assert_eq!(rt.state.filtered_connections().len(), 1);
    assert_eq!(rt.state.filtered_connections()[0].id, mysql_id);
}

#[test]
fn sqlite_workspace_hides_schema_dropdown_and_disables_multi_db_switching() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sqlite-dark");
    rt.frame(0.2);

    // Schema dropdown is hidden when SQLite has a single `main` schema.
    assert!(rt.find("sidebar-schema").is_none());
    assert!(has_text(&rt, "2 objects"));

    // Toolbar Open database button is disabled for SQLite and clicking it / Cmd+K does not open DB picker.
    let open_db_btn = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("toolbar-open-db"))
        .expect("toolbar-open-db button");
    assert!(!open_db_btn.focusable);
    let (ox, oy) = rt.find("toolbar-open-db").expect("toolbar-open-db coords");
    rt.pointer_down(ox, oy);
    rt.pointer_up(ox, oy);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));

    rt.key(KeyEvent {
        key: Key::Command('k'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));

    // Closing the table tab lands on stage-ready without "Switch Database (⌘K)" CTA.
    rt.key(KeyEvent {
        key: Key::Command('w'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    assert!(rt.find("stage-ready").is_some());
    assert!(!has_text(&rt, "Switch Database (⌘K)"));

    // Opening a query tab with a SQLite EXPLAIN plan hides "Run ANALYZE" button.
    rt.key(KeyEvent {
        key: Key::Command('t'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    rt.update(|s, _| {
        let plan = ExplainPlan::parse_sqlite_query_plan(
            "SELECT * FROM students".to_owned(),
            &[(2, 0, "SCAN students".to_owned())],
        )
        .expect("sqlite plan");
        if let Some(q) = s.active_session_mut().and_then(|sess| sess.active_query_tab_mut()) {
            q.explain_plan = Some(plan);
            q.show_explain = true;
        }
    });
    rt.frame(0.1);
    assert!(rt.find("explain-pane-root").is_some());
    assert!(rt.find("explain-copy-btn").is_some());
    assert!(rt.find("explain-analyze-btn").is_none());
}

fn empty_runtime(dark: bool) -> Runtime<AppState> {
    let mut state = AppState::init_in_memory(StoreData::default());
    state.appearance = if dark { ColorScheme::Dark } else { ColorScheme::Light };
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(dark, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt
}

fn click_button_by_label(rt: &mut Runtime<AppState>, label: &str) {
    let el = rt
        .elements()
        .into_iter()
        .find(|el| !el.exiting && !el.inert && el.role == "button" && el.label.as_deref() == Some(label))
        .unwrap_or_else(|| panic!("expected button with label {label:?}"));
    let x = el.rect[0] + el.rect[2] * 0.5;
    let y = el.rect[1] + el.rect[3] * 0.5;
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
}

#[test]
fn launcher_creating_first_connection_updates_empty_list_immediately() {
    let mut rt = empty_runtime(true);
    rt.frame(0.0);
    assert!(rt.find("launcher-empty").is_some());

    click_button_by_label(&mut rt, "New Connection");
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::ConnectionForm(_)));

    rt.key(KeyEvent::from(Key::Text("greendale_local".to_owned())));
    rt.frame(0.0);
    click_button_by_label(&mut rt, "Save");
    rt.frame(0.1);
    rt.frame(0.5);

    assert_eq!(rt.state.connections.len(), 1);
    let id = rt.state.connections[0].id;
    assert!(rt.find("launcher-empty").is_none());
    assert!(rt.find(&format!("launcher/conn/{id}")).is_some());
    assert!(has_text(&rt, "greendale_local"));
}

#[test]
fn launcher_new_connection_respects_and_reconciles_filters() {
    let mut rt = empty_runtime(true);
    rt.frame(0.0);

    // 1. Selecting an environment filter prefills new connection forms with that environment.
    let (dx, dy) = rt.find("launcher/filter/Development").expect("Development filter");
    rt.pointer_down(dx, dy);
    rt.pointer_up(dx, dy);
    rt.frame(0.1);

    rt.key(KeyEvent {
        key: Key::Command('n'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    match &rt.state.overlay {
        OverlayState::ConnectionForm(form) => {
            assert_eq!(form.env, EnvTag::Development);
            assert_eq!(form.color, EnvTag::Development.default_color());
        }
        other => panic!("expected ConnectionForm, got {other:?}"),
    }

    rt.key(KeyEvent::from(Key::Text("study_room_dev".to_owned())));
    rt.frame(0.0);
    click_button_by_label(&mut rt, "Save");
    rt.frame(0.1);
    rt.frame(0.5);

    let dev_id = rt
        .state
        .connections
        .iter()
        .find(|c| c.name == "study_room_dev")
        .expect("study_room_dev saved")
        .id;
    assert_eq!(rt.state.env_filter, Some(EnvTag::Development));
    assert!(rt.find("launcher-empty").is_none());
    assert!(rt.find(&format!("launcher/conn/{dev_id}")).is_some());
    assert!(has_text(&rt, "study_room_dev"));

    // 2. Importing a URL whose inferred env differs from the active env filter resets to All Connections.
    let (px, py) = rt.find("launcher/filter/Production").expect("Production filter");
    rt.pointer_down(px, py);
    rt.pointer_up(px, py);
    rt.frame(0.1);
    assert!(rt.find("launcher-empty").is_some());

    rt.key(KeyEvent {
        key: Key::Command('i'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("sqlite:///tmp/greendale_campus.db".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);
    click_button_by_label(&mut rt, "Save");
    rt.frame(0.1);
    rt.frame(0.5);

    let sqlite_id = rt
        .state
        .connections
        .iter()
        .find(|c| c.name == "greendale_campus")
        .expect("greendale_campus saved")
        .id;
    assert_eq!(rt.state.env_filter, None);
    assert!(rt.find("launcher-empty").is_none());
    assert!(rt.find(&format!("launcher/conn/{sqlite_id}")).is_some());
    assert!(has_text(&rt, "greendale_campus"));

    // 3. Changing the form's environment away from the active filter reconciles to All Connections on save.
    let (px, py) = rt.find("launcher/filter/Production").expect("Production filter");
    rt.pointer_down(px, py);
    rt.pointer_up(px, py);
    rt.frame(0.1);
    assert!(rt.find("launcher-empty").is_some());

    rt.key(KeyEvent {
        key: Key::Command('n'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    rt.update(|s, _| {
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.env = EnvTag::Staging;
            form.color = EnvTag::Staging.default_color();
        }
    });
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("campus_staging".to_owned())));
    rt.frame(0.0);
    click_button_by_label(&mut rt, "Save");
    rt.frame(0.1);
    rt.frame(0.5);

    let staging_id = rt
        .state
        .connections
        .iter()
        .find(|c| c.name == "campus_staging")
        .expect("campus_staging saved")
        .id;
    assert_eq!(rt.state.env_filter, None);
    assert!(rt.find("launcher-empty").is_none());
    assert!(rt.find(&format!("launcher/conn/{staging_id}")).is_some());
    assert!(has_text(&rt, "campus_staging"));

    // 4. A non-matching launcher search is cleared on save so the new connection is immediately visible.
    rt.update(|s, _| s.launcher_search = rune::TextEdit::new("dean_pelton_nonexistent"));
    rt.frame(0.1);
    assert!(rt.find("launcher-empty").is_some());

    rt.key(KeyEvent {
        key: Key::Command('n'),
        mods: Mods {
            cmd: true,
            ..Default::default()
        },
    });
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("troy_barnes_local".to_owned())));
    rt.frame(0.0);
    click_button_by_label(&mut rt, "Save");
    rt.frame(0.1);
    rt.frame(0.5);

    let local_id = rt
        .state
        .connections
        .iter()
        .find(|c| c.name == "troy_barnes_local")
        .expect("troy_barnes_local saved")
        .id;
    assert!(rt.state.launcher_search.text.is_empty());
    assert!(rt.find("launcher-empty").is_none());
    assert!(rt.find(&format!("launcher/conn/{local_id}")).is_some());
    assert!(has_text(&rt, "troy_barnes_local"));
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct MockSession {
    server_info: ServerInfo,
    disconnected: AtomicBool,
    half_open: AtomicBool,
    fail_next_sql: AtomicBool,
    fail_next_pre_dispatch: AtomicBool,
    fail_with_auth_on_disconnect: AtomicBool,
    ping_delay: Mutex<Duration>,
    pre_dispatch_delay: Mutex<Duration>,
    cancelled_keys: Mutex<Vec<u64>>,
    ping_calls: AtomicUsize,
    page_calls: AtomicUsize,
    describe_calls: AtomicUsize,
    sql_calls: AtomicUsize,
    pre_dispatch_entries: AtomicUsize,
    explain_calls: AtomicUsize,
    tx_calls: AtomicUsize,
}

impl MockSession {
    fn new(database: &str) -> Self {
        Self {
            server_info: ServerInfo {
                version: "PostgreSQL 18.4".to_owned(),
                current_database: database.to_owned(),
                current_user: "troy_barnes".to_owned(),
            },
            disconnected: AtomicBool::new(false),
            half_open: AtomicBool::new(false),
            fail_next_sql: AtomicBool::new(false),
            fail_next_pre_dispatch: AtomicBool::new(false),
            fail_with_auth_on_disconnect: AtomicBool::new(false),
            ping_delay: Mutex::new(Duration::ZERO),
            pre_dispatch_delay: Mutex::new(Duration::ZERO),
            cancelled_keys: Mutex::new(Vec::new()),
            ping_calls: AtomicUsize::new(0),
            page_calls: AtomicUsize::new(0),
            describe_calls: AtomicUsize::new(0),
            sql_calls: AtomicUsize::new(0),
            pre_dispatch_entries: AtomicUsize::new(0),
            explain_calls: AtomicUsize::new(0),
            tx_calls: AtomicUsize::new(0),
        }
    }

    fn set_disconnected(&self, value: bool) {
        self.disconnected.store(value, Ordering::Release);
    }

    fn set_half_open(&self, value: bool) {
        self.half_open.store(value, Ordering::Release);
    }

    fn set_fail_next_sql(&self, value: bool) {
        self.fail_next_sql.store(value, Ordering::Release);
    }

    fn set_fail_next_pre_dispatch(&self, value: bool) {
        self.fail_next_pre_dispatch.store(value, Ordering::Release);
    }

    fn set_fail_with_auth_on_disconnect(&self, value: bool) {
        self.fail_with_auth_on_disconnect.store(value, Ordering::Release);
    }

    fn set_ping_delay(&self, delay: Duration) {
        *self.ping_delay.lock().expect("ping_delay lock") = delay;
    }

    fn set_pre_dispatch_delay(&self, delay: Duration) {
        *self.pre_dispatch_delay.lock().expect("pre_dispatch_delay lock") = delay;
    }

    fn cancelled_keys(&self) -> Vec<u64> {
        self.cancelled_keys.lock().expect("cancelled_keys lock").clone()
    }

    fn check_transport(&self) -> Result<(), DbError> {
        if self.disconnected.load(Ordering::Acquire) || self.half_open.load(Ordering::Acquire) {
            if self.fail_with_auth_on_disconnect.load(Ordering::Acquire) {
                self.disconnected.store(true, Ordering::Release);
                return Err(DbError::AuthFailed("Access denied for user".to_owned()));
            }
            return Err(DbError::Disconnected);
        }
        Ok(())
    }
}

fn mock_columns_for(table: &str) -> Vec<ColumnMeta> {
    if table == "courses" {
        vec![
            ColumnMeta {
                name: "course_id".to_owned(),
                data_type: "integer".to_owned(),
                udt_name: "int4".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: true,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
            ColumnMeta {
                name: "title".to_owned(),
                data_type: "text".to_owned(),
                udt_name: "text".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: false,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
        ]
    } else {
        vec![
            ColumnMeta {
                name: "student_id".to_owned(),
                data_type: "integer".to_owned(),
                udt_name: "int4".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: true,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
            ColumnMeta {
                name: "full_name".to_owned(),
                data_type: "text".to_owned(),
                udt_name: "text".to_owned(),
                nullable: false,
                default_expr: None,
                is_primary_key: false,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
        ]
    }
}

impl DbSession for MockSession {
    fn engine(&self) -> DbEngine {
        DbEngine::Postgres
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    fn list_databases<'life0, 'async_trait>(
        &'life0 self,
    ) -> BoxFuture<'async_trait, Result<(Vec<DatabaseInfo>, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.check_transport()?;
            Ok((
                vec![
                    DatabaseInfo {
                        name: "postgres".to_owned(),
                        owner: "troy_barnes".to_owned(),
                        size_bytes: Some(8_192),
                    },
                    DatabaseInfo {
                        name: "greendale_idp_dev".to_owned(),
                        owner: "troy_barnes".to_owned(),
                        size_bytes: Some(65_536),
                    },
                ],
                ExecutedSql {
                    sql: "SELECT datname FROM pg_database".to_owned(),
                    duration_ms: 1,
                    error: None,
                },
            ))
        })
    }

    fn list_schemas<'life0, 'async_trait>(
        &'life0 self,
    ) -> BoxFuture<'async_trait, Result<(Vec<SchemaInfo>, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.check_transport()?;
            Ok((
                vec![
                    SchemaInfo {
                        name: "public".to_owned(),
                        is_system: false,
                    },
                    SchemaInfo {
                        name: "study_room".to_owned(),
                        is_system: false,
                    },
                ],
                ExecutedSql {
                    sql: "SELECT nspname FROM pg_namespace".to_owned(),
                    duration_ms: 1,
                    error: None,
                },
            ))
        })
    }

    fn list_objects<'life0, 'life1, 'async_trait>(
        &'life0 self,
        schema: &'life1 str,
    ) -> BoxFuture<'async_trait, Result<(Vec<DbObject>, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.check_transport()?;
            let objects = if schema == "study_room" {
                vec![
                    DbObject {
                        schema: "study_room".to_owned(),
                        name: "courses".to_owned(),
                        kind: DbObjectKind::Table,
                        estimated_rows: Some(1),
                        total_bytes: Some(16_384),
                    },
                    DbObject {
                        schema: "study_room".to_owned(),
                        name: "students".to_owned(),
                        kind: DbObjectKind::Table,
                        estimated_rows: Some(2),
                        total_bytes: Some(32_768),
                    },
                ]
            } else {
                vec![DbObject {
                    schema: schema.to_owned(),
                    name: "campus_news".to_owned(),
                    kind: DbObjectKind::Table,
                    estimated_rows: Some(5),
                    total_bytes: Some(8_192),
                }]
            };
            Ok((
                objects,
                ExecutedSql {
                    sql: format!("SELECT relname FROM pg_class WHERE nspname = '{schema}'"),
                    duration_ms: 1,
                    error: None,
                },
            ))
        })
    }

    fn list_columns<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        schema: &'life1 str,
        object: &'life2 str,
    ) -> BoxFuture<'async_trait, Result<(Vec<ColumnSummary>, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.check_transport()?;
            let cols = mock_columns_for(object)
                .into_iter()
                .map(|c| ColumnSummary {
                    name: c.name,
                    data_type: c.data_type,
                    nullable: c.nullable,
                    is_primary_key: c.is_primary_key,
                })
                .collect();
            Ok((
                cols,
                ExecutedSql {
                    sql: format!("SELECT attname FROM pg_attribute WHERE {schema}.{object}"),
                    duration_ms: 1,
                    error: None,
                },
            ))
        })
    }

    fn fetch_table_page<'life0, 'life1, 'async_trait>(
        &'life0 self,
        req: &'life1 PageRequest,
    ) -> BoxFuture<'async_trait, Result<(TablePage, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.page_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()?;
            let columns = mock_columns_for(&req.table);
            let rows = if req.table == "courses" {
                vec![vec![CellValue::Int(101), CellValue::Text("Spanish 101".to_owned())]]
            } else {
                vec![
                    vec![CellValue::Int(1), CellValue::Text("Troy Barnes".to_owned())],
                    vec![CellValue::Int(2), CellValue::Text("Abed Nadir".to_owned())],
                ]
            };
            let len = rows.len() as i64;
            let sql = format!("SELECT * FROM \"{}\".\"{}\"", req.schema, req.table);
            Ok((
                TablePage {
                    columns,
                    rows,
                    offset: req.offset,
                    limit: req.limit,
                    total_rows_estimate: Some(len),
                    total_is_exact: true,
                    has_more: false,
                    elapsed_ms: 2,
                },
                vec![ExecutedSql {
                    sql,
                    duration_ms: 2,
                    error: None,
                }],
            ))
        })
    }

    fn describe_table<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        schema: &'life1 str,
        table: &'life2 str,
    ) -> BoxFuture<'async_trait, Result<(TableStructure, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.describe_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()?;
            let columns = mock_columns_for(table);
            let pk_col = columns[0].name.clone();
            Ok((
                TableStructure {
                    columns,
                    indexes: vec![IndexInfo {
                        name: format!("{table}_pkey"),
                        columns: vec![pk_col.clone()],
                        is_unique: true,
                        is_primary: true,
                        method: "btree".to_owned(),
                        size_bytes: Some(8_192),
                        definition: format!("CREATE UNIQUE INDEX {table}_pkey ON {schema}.{table} ({pk_col})"),
                    }],
                    stats: TableStats {
                        total_bytes: Some(16_384),
                        data_bytes: Some(8_192),
                        index_bytes: Some(8_192),
                        estimated_rows: Some(2),
                        comment: None,
                        ddl: format!("CREATE TABLE \"{schema}\".\"{table}\" ();"),
                    },
                },
                vec![ExecutedSql {
                    sql: format!("DESCRIBE {schema}.{table}"),
                    duration_ms: 1,
                    error: None,
                }],
            ))
        })
    }

    fn execute_sql<'life0, 'life1, 'async_trait>(
        &'life0 self,
        statements: &'life1 [String],
        _row_limit: Option<usize>,
    ) -> BoxFuture<'async_trait, Result<(QueryBatchResult, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.sql_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_next_sql.swap(false, Ordering::AcqRel) {
                return Err(DbError::Disconnected);
            }
            self.check_transport()?;
            let mut results = Vec::new();
            let mut logs = Vec::new();
            for sql in statements {
                let is_select = sql.trim_start().to_ascii_uppercase().starts_with("SELECT");
                let (columns, rows, rows_affected) = if is_select {
                    (
                        vec![ColumnMeta {
                            name: "full_name".to_owned(),
                            data_type: "text".to_owned(),
                            udt_name: "text".to_owned(),
                            nullable: false,
                            default_expr: None,
                            is_primary_key: false,
                            is_unique: false,
                            comment: None,
                            foreign_key: None,
                        }],
                        vec![vec![CellValue::Text("Troy Barnes".to_owned())]],
                        None,
                    )
                } else {
                    (Vec::new(), Vec::new(), Some(1))
                };
                results.push(QueryStatementResult {
                    sql: sql.clone(),
                    columns,
                    rows,
                    rows_affected,
                    truncated: false,
                    total_rows_seen: 1,
                    elapsed_ms: 1,
                    error: None,
                });
                logs.push(ExecutedSql {
                    sql: sql.clone(),
                    duration_ms: 1,
                    error: None,
                });
            }
            Ok((
                QueryBatchResult {
                    results,
                    total_elapsed_ms: 1,
                    cancelled: false,
                },
                logs,
            ))
        })
    }

    fn execute_sql_cancellable<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        statements: &'life1 [String],
        row_limit: Option<usize>,
        _cancel_key: u64,
        cancel_flag: Option<&'life2 AtomicBool>,
    ) -> BoxFuture<'async_trait, Result<(QueryBatchResult, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.pre_dispatch_entries.fetch_add(1, Ordering::AcqRel);
            let delay = *self.pre_dispatch_delay.lock().expect("pre_dispatch_delay lock");
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if cancel_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                return Err(DbError::Cancelled);
            }
            if self.fail_next_pre_dispatch.swap(false, Ordering::AcqRel) {
                self.disconnected.store(true, Ordering::Release);
                return Err(DbError::DisconnectedBeforeDispatch(Box::new(DbError::Disconnected)));
            }
            self.execute_sql(statements, row_limit).await
        })
    }

    fn execute_transaction<'life0, 'life1, 'async_trait>(
        &'life0 self,
        statements: &'life1 [String],
    ) -> BoxFuture<'async_trait, Result<(TransactionOutcome, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.tx_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()?;
            let logs = statements
                .iter()
                .map(|sql| ExecutedSql {
                    sql: sql.clone(),
                    duration_ms: 1,
                    error: None,
                })
                .collect();
            Ok((
                TransactionOutcome {
                    statements_executed: statements.len(),
                    total_rows_affected: statements.len() as u64,
                    elapsed_ms: 2,
                },
                logs,
            ))
        })
    }

    fn explain_sql<'life0, 'life1, 'async_trait>(
        &'life0 self,
        statement: &'life1 str,
        _analyze: bool,
    ) -> BoxFuture<'async_trait, Result<(ExplainPlan, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.explain_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()?;
            Err(DbError::InvalidInput(format!("unused mock explain for {statement}")))
        })
    }

    fn fetch_fk_row<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        fk: &'life1 ForeignKeyRef,
        _value: &'life2 CellValue,
        _source_col: &'life3 ColumnMeta,
    ) -> BoxFuture<'async_trait, Result<(FkPeekRow, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.check_transport()?;
            Err(DbError::InvalidInput(format!("unused mock fk {}", fk.constraint_name)))
        })
    }

    fn cancel_running_query<'life0, 'async_trait>(
        &'life0 self,
    ) -> BoxFuture<'async_trait, Result<(), DbError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(()) })
    }

    fn cancel_query_by_key<'life0, 'async_trait>(
        &'life0 self,
        cancel_key: u64,
    ) -> BoxFuture<'async_trait, Result<(), DbError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.cancelled_keys.lock().expect("cancelled_keys lock").push(cancel_key);
            Ok(())
        })
    }

    fn ping<'life0, 'async_trait>(&'life0 self) -> BoxFuture<'async_trait, Result<(), DbError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let delay = *self.ping_delay.lock().expect("ping_delay lock");
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.ping_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()
        })
    }
}

struct MockCluster {
    connect_calls: Mutex<Vec<(Option<String>, Option<String>)>>,
    fail_connect: Mutex<Option<DbError>>,
    expected_password: Mutex<Option<String>>,
    sessions: Mutex<Vec<Arc<MockSession>>>,
    connect_delay: Duration,
}

impl MockCluster {
    fn new(connect_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            connect_calls: Mutex::new(Vec::new()),
            fail_connect: Mutex::new(None),
            expected_password: Mutex::new(None),
            sessions: Mutex::new(Vec::new()),
            connect_delay,
        })
    }

    fn connector(self: &Arc<Self>) -> SessionConnector {
        let cluster = Arc::clone(self);
        Arc::new(move |_profile, password, database| {
            let cluster = Arc::clone(&cluster);
            Box::pin(async move {
                if !cluster.connect_delay.is_zero() {
                    tokio::time::sleep(cluster.connect_delay).await;
                }
                cluster
                    .connect_calls
                    .lock()
                    .expect("connect_calls lock")
                    .push((password.clone(), database.clone()));

                if let Some(err) = cluster.fail_connect.lock().expect("fail_connect lock").clone() {
                    return Err(err);
                }
                if let Some(expected) = cluster
                    .expected_password
                    .lock()
                    .expect("expected_password lock")
                    .clone()
                    && password.as_deref() != Some(expected.as_str())
                {
                    return Err(DbError::AuthFailed("password authentication failed".to_owned()));
                }

                let db_name = database.unwrap_or_else(|| "postgres".to_owned());
                let sess = Arc::new(MockSession::new(&db_name));
                cluster.sessions.lock().expect("sessions lock").push(Arc::clone(&sess));
                Ok((
                    sess as Arc<dyn DbSession>,
                    vec![ExecutedSql {
                        sql: "SELECT version(), current_database(), current_user".to_owned(),
                        duration_ms: 1,
                        error: None,
                    }],
                ))
            })
        })
    }

    fn connect_count(&self) -> usize {
        self.connect_calls.lock().expect("connect_calls lock").len()
    }

    fn connect_calls(&self) -> Vec<(Option<String>, Option<String>)> {
        self.connect_calls.lock().expect("connect_calls lock").clone()
    }

    fn latest_session(&self) -> Arc<MockSession> {
        Arc::clone(
            self.sessions
                .lock()
                .expect("sessions lock")
                .last()
                .expect("at least one mock session"),
        )
    }

    fn set_fail_connect(&self, err: Option<DbError>) {
        *self.fail_connect.lock().expect("fail_connect lock") = err;
    }

    fn set_expected_password(&self, pw: Option<&str>) {
        *self.expected_password.lock().expect("expected_password lock") = pw.map(str::to_owned);
    }
}

fn worker_runtime(cluster: &Arc<MockCluster>) -> Runtime<AppState> {
    let mut store = sample_store_data();
    store.connections.retain(|c| c.name == "greendale_local");
    store.connections[0].database = None;
    let mut state = AppState::init_in_memory(store);
    state.appearance = ColorScheme::Dark;
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn mock worker"));
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt
}

fn poll_until(
    rt: &mut Runtime<AppState>,
    timeout: Duration,
    mut pred: impl FnMut(&mut Runtime<AppState>) -> bool,
) {
    let deadline = Instant::now() + timeout;
    loop {
        rt.frame(0.02);
        if pred(rt) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for async UI condition"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn connect_select_db_schema_and_students(rt: &mut Runtime<AppState>) {
    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    poll_until(rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
        })
    });

    rt.update(|s, _| s.select_schema("study_room".to_owned()));
    poll_until(rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some_and(|c| {
                c.active_schema.as_deref() == Some("study_room")
                    && c.objects.iter().any(|o| o.name == "students")
            })
    });

    let (sx, sy) = rt
        .find("sidebar/obj/study_room.students")
        .expect("students sidebar item");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    poll_until(rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| t.object_name == "students" && t.page.is_some() && t.structure.is_some())
    });
}

#[test]
fn idle_disconnect_silently_reconnects_in_background_on_table_click() {
    let cluster = MockCluster::new(Duration::from_millis(25));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    let connects_before = cluster.connect_count();

    // Simulate the database closing the idle socket while the app sat idle.
    cluster.latest_session().set_disconnected(true);

    let (cx, cy) = rt
        .find("sidebar/obj/study_room.courses")
        .expect("courses sidebar item");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        assert!(
            rt.find("stage-disconnected").is_none(),
            "stage-disconnected must never flash during silent reconnect"
        );
        let sess = rt.state.active_session().expect("session");
        assert!(sess.status.as_connected().is_some());
        sess.active_table_tab()
            .is_some_and(|t| t.object_name == "courses" && t.page.is_some() && t.structure.is_some())
    });

    // Concurrent FetchTablePage + DescribeTable coalesced into a single reconnect targeting the active DB.
    assert_eq!(cluster.connect_count(), connects_before + 1);
    assert_eq!(
        cluster.connect_calls().last().and_then(|(_, db)| db.as_deref()),
        Some("greendale_idp_dev")
    );

    let sess = rt.state.active_session().expect("session");
    assert!(!sess.console_entries.iter().any(|entry| {
        entry
            .error
            .as_deref()
            .is_some_and(|err| err.contains("database session is disconnected"))
    }));
}

#[test]
fn half_open_disconnect_silently_reconnects_for_reads_sql_and_staged_commit() {
    let cluster = MockCluster::new(Duration::from_millis(20));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);

    // 1. Half-open drop on table page read (is_disconnected() is false, wire query fails with Disconnected).
    let mut connects = cluster.connect_count();
    let stale_read = cluster.latest_session();
    stale_read.set_half_open(true);
    let (cx, cy) = rt
        .find("sidebar/obj/study_room.courses")
        .expect("courses sidebar item");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| t.object_name == "courses" && t.page.is_some() && t.structure.is_some())
    });
    assert_eq!(cluster.connect_count(), connects + 1);
    connects = cluster.connect_count();

    // 2. Half-open drop on read-only SQL execution retries transparently and succeeds.
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.1);
    cluster.latest_session().set_half_open(true);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT full_name FROM study_room.students;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && !q.results.is_empty())
    });
    assert_eq!(cluster.connect_count(), connects + 1);
    let qtab = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab");
    assert!(qtab.batch_error.is_none());
    assert_eq!(qtab.results[0].statement.rows.len(), 1);
    connects = cluster.connect_count();

    // 3. Half-open drop before staged commit runs pre-flight ping, reconnects before BEGIN, and executes tx once.
    let students_tab_id = rt
        .state
        .active_session()
        .expect("session")
        .tabs
        .iter()
        .find(|t| t.schema_and_name() == Some(("study_room", "students")))
        .expect("students tab")
        .id;
    rt.update(|s, tx| s.activate_tab(students_tab_id, tx));
    rt.frame(0.1);
    rt.update(|s, tx| {
        assert!(s.begin_cell_edit(RowKey::Existing(0), 1, tx));
    });
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("Troy 'T-Bone' Barnes".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    let stale_tx_sess = cluster.latest_session();
    stale_tx_sess.set_half_open(true);
    rt.update(|s, _| s.commit_staged_changes());
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| !t.committing && !t.staged.has_changes() && !t.page_loading)
    });
    assert_eq!(cluster.connect_count(), connects + 1);
    assert!(stale_tx_sess.ping_calls.load(Ordering::Acquire) >= 1);
    assert_eq!(stale_tx_sess.tx_calls.load(Ordering::Acquire), 0);
    assert_eq!(cluster.latest_session().tx_calls.load(Ordering::Acquire), 1);
    connects = cluster.connect_count();

    // 4. Half-open drop before mutating SQL runs pre-flight ping, reconnects, and executes UPDATE once.
    let query_tab_id = rt
        .state
        .active_session()
        .expect("session")
        .tabs
        .iter()
        .find(|t| t.as_query().is_some())
        .expect("query tab")
        .id;
    rt.update(|s, tx| s.activate_tab(query_tab_id, tx));
    rt.frame(0.1);
    let stale_write_sess = cluster.latest_session();
    stale_write_sess.set_half_open(true);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("UPDATE study_room.students SET full_name = 'Troy' WHERE student_id = 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| r.statement.rows_affected == Some(1)))
    });
    assert_eq!(cluster.connect_count(), connects + 1);
    assert!(stale_write_sess.ping_calls.load(Ordering::Acquire) >= 1);
    assert_eq!(stale_write_sess.sql_calls.load(Ordering::Acquire), 0);
    assert_eq!(cluster.latest_session().sql_calls.load(Ordering::Acquire), 1);
}

#[test]
fn failed_silent_reconnect_and_manual_reconnect_restores_database_schema_and_tab() {
    let cluster = MockCluster::new(Duration::from_millis(25));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    let connects_before = cluster.connect_count();

    // Drop session AND reject reconnect while server is unreachable.
    cluster.latest_session().set_disconnected(true);
    cluster.set_fail_connect(Some(DbError::ConnectionFailed("Connection refused".to_owned())));

    let (cx, cy) = rt
        .find("sidebar/obj/study_room.courses")
        .expect("courses sidebar item");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.find("stage-disconnected").is_some()
            && rt
                .state
                .active_session()
                .is_some_and(|s| matches!(s.status, ConnectionState::Disconnected { .. }))
    });

    // Only one silent reconnect attempt happened for concurrent FetchTablePage + DescribeTable.
    assert_eq!(cluster.connect_count(), connects_before + 1);
    match &rt.state.active_session().expect("session").status {
        ConnectionState::Disconnected { reason: Some(reason) } => {
            assert!(reason.contains("Connection refused"), "unexpected reason: {reason}");
        }
        other => panic!("expected Disconnected with Connection refused, got {other:?}"),
    }

    // Bring server back online and click the Reconnect button in the disconnected stage.
    cluster.set_fail_connect(None);
    let stage_rect = rt
        .elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some("stage-disconnected"))
        .expect("stage-disconnected")
        .rect;
    let reconnect_btn = rt
        .elements()
        .into_iter()
        .find(|el| {
            !el.exiting
                && !el.inert
                && el.role == "button"
                && el.label.as_deref() == Some("Reconnect")
                && el.rect[0] >= stage_rect[0]
                && el.rect[1] >= stage_rect[1]
                && el.rect[0] + el.rect[2] <= stage_rect[0] + stage_rect[2]
                && el.rect[1] + el.rect[3] <= stage_rect[1] + stage_rect[3]
        })
        .expect("Reconnect button inside stage-disconnected");
    rt.pointer_down(
        reconnect_btn.rect[0] + reconnect_btn.rect[2] * 0.5,
        reconnect_btn.rect[1] + reconnect_btn.rect[3] * 0.5,
    );
    rt.pointer_up(
        reconnect_btn.rect[0] + reconnect_btn.rect[2] * 0.5,
        reconnect_btn.rect[1] + reconnect_btn.rect[3] * 0.5,
    );

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        assert!(rt.find("stage-no-db").is_none(), "must never return to Open Database screen");
        let sess = rt.state.active_session().expect("session");
        sess.status.as_connected().is_some()
            && sess
                .active_table_tab()
                .is_some_and(|t| t.object_name == "courses" && t.page.is_some() && t.structure.is_some())
    });

    let sess = rt.state.active_session().expect("session");
    let connected = sess.status.as_connected().expect("connected");
    assert_eq!(connected.active_database.as_deref(), Some("greendale_idp_dev"));
    assert_eq!(connected.active_schema.as_deref(), Some("study_room"));
    assert_eq!(
        connected.selected_object,
        Some(("study_room".to_owned(), "courses".to_owned()))
    );
    assert!(
        sess.tabs
            .iter()
            .any(|t| t.schema_and_name() == Some(("study_room", "students"))),
        "previous students tab must remain open"
    );
}

#[test]
fn explicit_disconnect_stays_disconnected_and_reconnect_restores_context() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    let connects_before = cluster.connect_count();

    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Disconnect, tx));
    });
    rt.frame(0.2);
    assert!(rt.find("stage-disconnected").is_some());

    // Any in-flight or stray reload while explicitly disconnected must NOT silently reconnect.
    rt.update(|s, _| s.reload_active_tab());
    std::thread::sleep(Duration::from_millis(50));
    rt.frame(0.1);
    assert_eq!(cluster.connect_count(), connects_before);
    assert!(rt.find("stage-disconnected").is_some());

    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        assert!(rt.find("stage-no-db").is_none());
        let sess = rt.state.active_session().expect("session");
        sess.status.as_connected().is_some()
            && sess
                .active_table_tab()
                .is_some_and(|t| t.object_name == "students" && t.page.is_some() && t.structure.is_some())
    });

    let connected = rt
        .state
        .active_session()
        .expect("session")
        .status
        .as_connected()
        .expect("connected");
    assert_eq!(connected.active_database.as_deref(), Some("greendale_idp_dev"));
    assert_eq!(connected.active_schema.as_deref(), Some("study_room"));
}

#[test]
fn ask_every_time_password_prompt_reconnects_in_place_preserving_context() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    cluster.set_expected_password(Some("dean_pelton_secret"));
    let mut rt = worker_runtime(&cluster);
    rt.update(|s, _| {
        s.connections[0].password_mode = PasswordMode::AskEveryTime;
    });
    rt.frame(0.0);

    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::PasswordPrompt(_)));

    rt.key(KeyEvent::from(Key::Text("dean_pelton_secret".to_owned())));
    rt.frame(0.0);
    rt.update(|s, tx| s.submit_password_prompt(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
        })
    });
    rt.update(|s, _| s.select_schema("study_room".to_owned()));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some_and(|c| c.active_schema.as_deref() == Some("study_room"))
    });
    let (sx, sy) = rt
        .find("sidebar/obj/study_room.students")
        .expect("students sidebar item");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| t.page.is_some() && t.structure.is_some())
    });

    let original_sid = rt.state.active_session().expect("session").id;

    // Disconnect clears session_password; triggering Reconnect opens PasswordPrompt targeting original_sid.
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Disconnect, tx));
    });
    rt.frame(0.1);
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });
    rt.frame(0.1);
    match &rt.state.overlay {
        OverlayState::PasswordPrompt(prompt) => {
            assert_eq!(prompt.session_id, Some(original_sid));
            assert_eq!(prompt.database_override.as_deref(), Some("greendale_idp_dev"));
            assert_eq!(prompt.preferred_schema.as_deref(), Some("study_room"));
        }
        other => panic!("expected PasswordPrompt overlay, got {other:?}"),
    }

    rt.key(KeyEvent::from(Key::Text("dean_pelton_secret".to_owned())));
    rt.frame(0.0);
    rt.update(|s, tx| s.submit_password_prompt(tx));

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
            && rt
                .state
                .active_session()
                .and_then(|s| s.active_table_tab())
                .is_some_and(|t| t.page.is_some() && t.structure.is_some())
    });

    let sess = rt.state.active_session().expect("session");
    assert_eq!(sess.id, original_sid);
    let connected = sess.status.as_connected().expect("connected");
    assert_eq!(connected.active_database.as_deref(), Some("greendale_idp_dev"));
    assert_eq!(connected.active_schema.as_deref(), Some("study_room"));
    assert_eq!(
        sess.active_table_tab().map(|t| t.object_name.as_str()),
        Some("students")
    );
}

#[test]
fn superseded_write_does_not_execute_on_switched_database_or_disconnect_it() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let db_a_sess = cluster.latest_session();
    db_a_sess.set_ping_delay(Duration::from_millis(120));
    db_a_sess.set_half_open(true);

    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("UPDATE study_room.students SET full_name = 'Troy' WHERE student_id = 1;");
        assert!(s.run_active_query(true));
    });
    std::thread::sleep(Duration::from_millis(20));
    rt.frame(0.02);

    // Switch database while DB A's preflight ping is still pending and destined to fail.
    rt.update(|s, tx| s.select_database("postgres".to_owned(), tx));

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("postgres"))
                && s.active_query_tab().is_some_and(|q| !q.running)
        })
    });

    // Pump additional frames past DB A's ping delay to ensure delayed events do not drop DB B.
    let settle_until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < settle_until {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(rt.find("stage-disconnected").is_none());
    let sess = rt.state.active_session().expect("session");
    let connected = sess.status.as_connected().expect("postgres stays connected");
    assert_eq!(connected.active_database.as_deref(), Some("postgres"));
    assert_eq!(cluster.latest_session().sql_calls.load(Ordering::Acquire), 0);
}

#[test]
fn mid_flight_sql_does_not_replay_and_cancel_during_reconnect_skips_dispatch() {
    let cluster = MockCluster::new(Duration::from_millis(120));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // Part A: Mid-flight drop after preflight ping passes must NOT replay uncertain SQL like nextval.
    let seq_sess = cluster.latest_session();
    seq_sess.set_fail_next_sql(true);
    let connects_before = cluster.connect_count();

    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT nextval('study_room.students_id_seq');");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.find("stage-disconnected").is_some()
            && rt
                .state
                .active_session()
                .and_then(|s| s.active_query_tab())
                .is_some_and(|q| !q.running)
    });
    assert_eq!(cluster.connect_count(), connects_before);
    assert_eq!(seq_sess.sql_calls.load(Ordering::Acquire), 1);

    // Part B: Cancelling a query while silent reconnect is in progress prevents dispatch onto replacement.
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    cluster.latest_session().set_disconnected(true);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 42;");
        assert!(s.run_active_query(true));
    });
    std::thread::sleep(Duration::from_millis(20));
    rt.frame(0.02);

    rt.update(|s, _| {
        assert!(s.cancel_active_query());
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.batch_cancelled)
    });

    let settle_until = Instant::now() + Duration::from_millis(160);
    while Instant::now() < settle_until {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(cluster.latest_session().sql_calls.load(Ordering::Acquire), 0);

    // Part C: Cancelling during pre-dispatch checkout/ping skips wire SQL dispatch and targets req key.
    let connects_before_c = cluster.connect_count();
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        cluster.connect_count() == connects_before_c + 1
            && rt
                .state
                .active_session()
                .and_then(|s| s.status.as_connected())
                .is_some()
    });

    let active_sess = cluster.latest_session();
    let connects_before_c_query = cluster.connect_count();
    active_sess.set_pre_dispatch_delay(Duration::from_millis(120));
    let mut cancelled_req_id = None;
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 99;");
        assert!(s.run_active_query(true));
        cancelled_req_id = s
            .active_session()
            .and_then(|sess| sess.active_query_tab())
            .and_then(|qt| qt.active_req_id)
            .map(tv_core::RequestId::get);
    });
    poll_until(&mut rt, Duration::from_secs(3), |_| {
        active_sess.pre_dispatch_entries.load(Ordering::Acquire) == 1
    });

    rt.update(|s, _| {
        assert!(s.cancel_active_query());
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.batch_cancelled)
            && !active_sess.cancelled_keys().is_empty()
    });

    let settle_part_c = Instant::now() + Duration::from_millis(150);
    while Instant::now() < settle_part_c {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(cluster.connect_count(), connects_before_c_query);
    assert_eq!(active_sess.pre_dispatch_entries.load(Ordering::Acquire), 1);
    assert_eq!(active_sess.sql_calls.load(Ordering::Acquire), 0);
    assert_eq!(
        active_sess.cancelled_keys(),
        vec![cancelled_req_id.expect("active_req_id")]
    );
}

#[test]
fn mysql_pool_replacement_auth_failure_prompts_ask_every_time() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    cluster.set_expected_password(Some("old_secret"));
    let mut rt = worker_runtime(&cluster);
    rt.update(|s, _| {
        s.connections[0].password_mode = PasswordMode::AskEveryTime;
    });
    rt.frame(0.0);

    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("old_secret".to_owned())));
    rt.frame(0.0);
    rt.update(|s, tx| s.submit_password_prompt(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
        })
    });
    rt.update(|s, _| s.select_schema("study_room".to_owned()));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some_and(|c| c.active_schema.as_deref() == Some("study_room"))
    });
    let (sx, sy) = rt
        .find("sidebar/obj/study_room.students")
        .expect("students sidebar item");
    rt.pointer_down(sx, sy);
    rt.pointer_up(sx, sy);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| t.page.is_some() && t.structure.is_some())
    });

    let original_sid = rt.state.active_session().expect("session").id;

    // Simulate rotated DB password causing replacement checkout auth failure on dead pool socket.
    cluster.set_expected_password(Some("new_secret"));
    let stale_sess = cluster.latest_session();
    stale_sess.set_half_open(true);
    stale_sess.set_fail_with_auth_on_disconnect(true);

    let (cx, cy) = rt
        .find("sidebar/obj/study_room.courses")
        .expect("courses sidebar item");
    rt.pointer_down(cx, cy);
    rt.pointer_up(cx, cy);

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        matches!(rt.state.overlay, OverlayState::PasswordPrompt(_))
    });
    match &rt.state.overlay {
        OverlayState::PasswordPrompt(prompt) => {
            assert_eq!(prompt.session_id, Some(original_sid));
            assert_eq!(prompt.database_override.as_deref(), Some("greendale_idp_dev"));
            assert_eq!(prompt.preferred_schema.as_deref(), Some("study_room"));
            assert!(prompt.error.is_some());
        }
        other => panic!("expected PasswordPrompt overlay, got {other:?}"),
    }

    rt.key(KeyEvent::from(Key::Text("new_secret".to_owned())));
    rt.frame(0.0);
    rt.update(|s, tx| s.submit_password_prompt(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
            && rt
                .state
                .active_session()
                .and_then(|s| s.active_table_tab())
                .is_some_and(|t| t.object_name == "courses" && t.page.is_some() && t.structure.is_some())
    });
}

#[test]
fn reconnect_preserves_none_selected_object_after_schema_switch() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, _| s.select_schema("public".to_owned()));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some_and(|c| c.active_schema.as_deref() == Some("public") && c.selected_object.is_none())
    });

    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Disconnect, tx));
    });
    rt.frame(0.1);
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    let connected = rt
        .state
        .active_session()
        .expect("session")
        .status
        .as_connected()
        .expect("connected");
    assert_eq!(connected.active_schema.as_deref(), Some("public"));
    assert_eq!(connected.selected_object, None);
}

#[test]
fn disconnect_and_manual_reconnect_ignores_stale_query_from_reused_slot() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let stale_sess = cluster.latest_session();
    stale_sess.set_ping_delay(Duration::from_millis(120));
    stale_sess.set_half_open(true);

    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("UPDATE study_room.students SET full_name = 'Troy' WHERE student_id = 1;");
        assert!(s.run_active_query(true));
    });
    std::thread::sleep(Duration::from_millis(20));
    rt.frame(0.02);

    // Explicit disconnect drops worker entry, then manual reconnect creates a fresh entry with new req ID.
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Disconnect, tx));
    });
    std::thread::sleep(Duration::from_millis(20));
    rt.frame(0.02);
    rt.update(|s, tx| {
        assert!(dispatch_action(s, Action::Reconnect, tx));
    });

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
            && rt
                .state
                .active_session()
                .and_then(|s| s.active_query_tab())
                .is_some_and(|q| !q.running)
    });

    let settle_until = Instant::now() + Duration::from_millis(160);
    while Instant::now() < settle_until {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(rt.find("stage-disconnected").is_none());
    let sess = rt.state.active_session().expect("session");
    assert!(sess.status.as_connected().is_some());
    assert_eq!(cluster.latest_session().sql_calls.load(Ordering::Acquire), 0);
}

#[test]
fn pre_dispatch_disconnect_reconnects_write_once_and_failed_db_switch_keeps_prior_db() {
    let cluster = MockCluster::new(Duration::from_millis(10));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // Part A: Pre-dispatch disconnect (MySQL checked-out socket ping failure) reconnects and runs UPDATE once.
    let before_s5 = cluster.connect_count();
    let pre_drop_sess = cluster.latest_session();
    pre_drop_sess.set_fail_next_pre_dispatch(true);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("UPDATE study_room.students SET full_name = 'Abed' WHERE student_id = 2;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| r.statement.rows_affected == Some(1)))
    });
    assert!(rt.find("stage-disconnected").is_none());
    assert_eq!(cluster.connect_count(), before_s5 + 1);
    assert_eq!(pre_drop_sess.sql_calls.load(Ordering::Acquire), 0);
    assert_eq!(cluster.latest_session().sql_calls.load(Ordering::Acquire), 1);

    // Part B: Failed DB switch preserves prior connected DB and active_connect_req_id so subsequent SQL works.
    cluster.set_fail_connect(Some(DbError::ConnectionFailed("permission denied".to_owned())));
    rt.update(|s, tx| s.select_database("postgres".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|s| s.pending_db_switch_req.is_none())
    });
    {
        let sess = rt.state.active_session().expect("session");
        let connected = sess.status.as_connected().expect("still connected");
        assert_eq!(connected.active_database.as_deref(), Some("greendale_idp_dev"));
    }

    cluster.set_fail_connect(None);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT full_name FROM study_room.students;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| !r.statement.rows.is_empty()))
    });

    // Part C: EXPLAIN ANALYZE from DB A superseded by DB switch must not execute on DB B.
    let db_a = cluster.latest_session();
    db_a.set_ping_delay(Duration::from_millis(120));
    db_a.set_half_open(true);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("UPDATE study_room.students SET full_name = 'Annie' WHERE student_id = 2;");
        assert!(s.run_active_query_explain(true));
    });
    std::thread::sleep(Duration::from_millis(20));
    rt.frame(0.02);

    rt.update(|s, tx| s.select_database("postgres".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("postgres"))
                && s.active_query_tab().is_some_and(|q| !q.explain_loading)
        })
    });

    let settle_until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < settle_until {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rt.find("stage-disconnected").is_none());
    assert_eq!(cluster.latest_session().explain_calls.load(Ordering::Acquire), 0);
}

#[test]
fn cancel_after_database_switch_targets_executing_session_only() {
    let cluster = MockCluster::new(Duration::ZERO);
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let sess_a = cluster.latest_session();
    sess_a.set_pre_dispatch_delay(Duration::from_millis(250));
    let mut query_req_id = None;
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
        query_req_id = s
            .active_session()
            .and_then(|sess| sess.active_query_tab())
            .and_then(|qt| qt.active_req_id)
            .map(tv_core::RequestId::get);
    });
    let query_req_id = query_req_id.expect("query_req_id");
    poll_until(&mut rt, Duration::from_secs(3), |_| {
        sess_a.pre_dispatch_entries.load(Ordering::Acquire) == 1
    });

    rt.update(|s, tx| s.select_database("postgres".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("postgres"))
        })
    });
    let sess_b = cluster.latest_session();
    assert!(!Arc::ptr_eq(&sess_a, &sess_b));

    rt.update(|s, _| {
        assert!(s.cancel_active_query());
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.batch_cancelled)
            && !sess_a.cancelled_keys().is_empty()
    });

    let settle_until = Instant::now() + Duration::from_millis(120);
    while Instant::now() < settle_until {
        rt.frame(0.02);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(sess_a.cancelled_keys(), vec![query_req_id]);
    assert!(sess_b.cancelled_keys().is_empty());
}

#[test]
fn select_schema_during_database_switch_does_not_drop_switch() {
    let cluster = MockCluster::new(Duration::from_millis(120));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| {
        s.select_database("greendale_idp_dev".to_owned(), tx);
        assert!(
            s.active_session()
                .is_some_and(|sess| sess.pending_db_switch_req.is_some())
        );
        s.select_schema("public".to_owned());
    });

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
        })
    });

    let db_b_sess = cluster.latest_session();
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| !r.statement.rows.is_empty()))
    });

    assert_eq!(db_b_sess.sql_calls.load(Ordering::Acquire), 1);
    assert!(rt.find("stage-disconnected").is_none());
}

#[test]
fn queued_db_b_success_preserved_when_db_c_fails() {
    let cluster = MockCluster::new(Duration::ZERO);
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    // Request DB B and wait for the worker to finish installing B and queuing its event BEFORE pumping UI frames.
    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    let wait_deadline = Instant::now() + Duration::from_secs(3);
    while cluster.connect_count() < 2 {
        assert!(Instant::now() < wait_deadline, "timed out waiting for DB B connect");
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(25));

    cluster.set_fail_connect(Some(DbError::ConnectionFailed("db C denied".to_owned())));
    rt.update(|s, tx| s.select_database("study_room_c".to_owned(), tx));

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|s| s.pending_db_switch_req.is_none())
    });
    assert_eq!(
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .and_then(|c| c.active_database.as_deref()),
        Some("greendale_idp_dev")
    );

    cluster.set_fail_connect(None);
    let db_b_sess = cluster.latest_session();
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| !r.statement.rows.is_empty()))
    });
    assert_eq!(db_b_sess.sql_calls.load(Ordering::Acquire), 1);
    assert!(rt.find("stage-disconnected").is_none());
}

#[test]
fn background_disconnect_of_db_a_does_not_drop_in_flight_switch_to_db_b() {
    let cluster = MockCluster::new(Duration::from_millis(140));
    let mut rt = worker_runtime(&cluster);
    rt.frame(0.0);

    let conn_id = rt.state.connections[0].id;
    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let sess_a = cluster.latest_session();
    sess_a.set_pre_dispatch_delay(Duration::from_millis(40));
    sess_a.set_fail_next_sql(true);

    rt.update(|s, tx| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
        s.select_database("greendale_idp_dev".to_owned(), tx);
    });

    // DB A's mid-flight disconnect reaches the UI before DB B's 140ms connect finishes.
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.find("stage-disconnected").is_some()
            && rt
                .state
                .active_session()
                .is_some_and(|s| s.pending_db_switch_req.is_some())
    });

    // DB B's switch still completes cleanly and transitions the session back to Connected on B.
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|s| {
            s.pending_db_switch_req.is_none()
                && s.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
        })
    });
    assert!(rt.find("stage-disconnected").is_none());

    let db_b_sess = cluster.latest_session();
    assert!(!Arc::ptr_eq(&sess_a, &db_b_sess));
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .and_then(|sess| sess.active_query_tab_mut())
            .expect("query tab");
        q.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|q| !q.running && q.results.first().is_some_and(|r| !r.statement.rows.is_empty()))
    });
    assert_eq!(db_b_sess.sql_calls.load(Ordering::Acquire), 1);
    assert!(rt.find("stage-disconnected").is_none());
}
