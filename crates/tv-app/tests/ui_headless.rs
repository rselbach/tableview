#![allow(clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::{ColorScheme, Env, GesturePhase, Key, KeyEvent, Mods, Runtime, WheelEvent};
use rune_kit::diff::DiffMode;
use tv_app::platform::{Action, dispatch_action};
use tv_app::scenes::{reset_to_sample_state, sample_store_data};
use tv_app::state::{
    AppState, CONSOLE_DEFAULT_HEIGHT, CONSOLE_MIN_HEIGHT, CONSOLE_MIN_STAGE_HEIGHT, ConnectionState,
    ConsoleLogEntry, OverlayState, PickerKind, RowKey, Screen, SessionState, SidebarTab, TableViewMode,
};
use tv_app::views;
use tv_app::worker::{CredentialBackend, SessionConnector, WorkerBridge, WorkerCmd};
use tv_core::{
    AppearancePreference, CellValue, ColumnMeta, ColumnSummary, ConnectionProfile, ConnectionStore, DatabaseInfo,
    DbEngine, DbError, DbObject, DbObjectKind, DbSession, EnvTag, ExecutedSql, ExplainPlan, ExportFormat, FkPeekRow,
    ForeignKeyRef, IndexInfo, MysqlQuoteMode, PageRequest, PasswordMode, PostgresSessionMode, QueryBatchResult,
    QueryStatementResult, SafetyPolicy, SchemaInfo, ServerInfo, SettingsStore, SqlSessionMode, StoreData, TablePage,
    TableStats, TableStructure, TransactionOutcome, connect_session,
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
            download_url: "https://github.com/rselbach/TableView/releases/download/v0.3.0/TableView-0.3.0-macos-arm64.dmg".to_owned(),
            notes_url: Some("https://github.com/rselbach/TableView/releases/tag/v0.3.0".to_owned()),
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

    // Startup recovery state when an earlier restart-to-update failed and older build booted.
    rt.update(|s, _| {
        s.update.snapshot.status = UpdateStatus::Failed(UpdateFailure::RelaunchRecovery {
            version: "0.2.0".to_owned(),
            log_path: "/tmp/relaunch.log".to_owned(),
        });
    });
    rt.frame(0.1);
    assert!(rt.find("update-dock-card").is_some());
    assert!(rt.find("sidebar-toggle-unread").is_some());
    assert!(rt.find("update-card-retry-btn").is_some());
    assert!(has_text(&rt, "v0.2.0 Update Incomplete"));

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
    let store_path = dir.path().join("connections.json");
    rt.update(|s, tx| {
        s.store_path = store_path;
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
    let pg_id = rt.state.connections[0].id;
    let mysql_prof = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy_barnes");
    let sqlite_prof = ConnectionProfile::new_sqlite("greendale_sqlite", "/tmp/greendale.sqlite3");
    let mysql_id = mysql_prof.id;
    let sqlite_id = sqlite_prof.id;
    rt.update(|s, _| {
        s.connections.push(mysql_prof);
        s.connections.push(sqlite_prof);
    });
    rt.frame(0.1);

    assert!(!has_text(&rt, "PG"));
    assert!(!has_text(&rt, "MY"));
    assert!(!has_text(&rt, "SL"));
    assert!(rt.find(&format!("launcher/conn/{pg_id}/emblem")).is_some());
    assert!(rt.find(&format!("launcher/conn/{mysql_id}/emblem")).is_some());
    assert!(rt.find(&format!("launcher/conn/{sqlite_id}/emblem")).is_some());

    let tree = rt.accessibility_tree();
    let has_label = |label: &str| tree.nodes.iter().any(|(_, n)| n.label() == Some(label));
    assert!(has_label("PostgreSQL"));
    assert!(has_label("MySQL"));
    assert!(has_label("SQLite"));
    assert!(has_text(&rt, "/tmp/greendale.sqlite3"));

    // Engine catalog helper maps each engine to a distinct 24x24 symbol.
    let pg_icon = tv_app::icons::engine(DbEngine::Postgres);
    let my_icon = tv_app::icons::engine(DbEngine::Mysql);
    let sl_icon = tv_app::icons::engine(DbEngine::Sqlite);
    assert_ne!(pg_icon.name, my_icon.name);
    assert_ne!(my_icon.name, sl_icon.name);
    assert_ne!(pg_icon.name, sl_icon.name);

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

fn find_el_by_key<'a>(els: &'a [rune::ElementInfo], key: &str) -> &'a rune::ElementInfo {
    els.iter()
        .find(|el| el.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("missing element key {key}"))
}

fn find_el_by_path_suffix<'a>(els: &'a [rune::ElementInfo], suffix: &str) -> Option<&'a rune::ElementInfo> {
    els.iter().find(|el| el.path.ends_with(suffix))
}

fn settle_frames(rt: &mut Runtime<AppState>) {
    for _ in 0..4 {
        rt.frame(1.0 / 60.0);
    }
}

#[test]
fn sql_results_horizontal_scrollbar_drag_wheel_shift_wheel_and_keys() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-editor-dark");
    settle_frames(&mut rt);

    let els = rt.elements();
    let track = find_el_by_key(&els, "tv-qres-3-0-hscroll-track");
    let thumb = find_el_by_key(&els, "tv-qres-3-0-hscroll-thumb");
    assert!(track.rect[2] > 200.0, "expected wide hscroll track, got {:?}", track.rect);
    assert!(
        thumb.rect[2] >= 32.0 && thumb.rect[2] < track.rect[2],
        "expected overflowing hscroll thumb < track width, thumb={:?} track={:?}",
        thumb.rect,
        track.rect
    );
    let track_rect = track.rect;
    let start_thumb_x = thumb.rect[0];
    let mid_y = thumb.rect[1] + thumb.rect[3] * 0.5;
    let grab_x = thumb.rect[0] + thumb.rect[2] * 0.5;
    let right_target_x = track_rect[0] + track_rect[2] - 8.0;

    // 1. Drag scrollbar thumb to the right edge -> rightmost column `group_id` (`c-8`) appears
    // while frozen `#` column stays pinned at the left edge.
    rt.pointer_down(grab_x, mid_y);
    rt.frame(0.0);
    rt.pointer_move(right_target_x, mid_y);
    rt.frame(0.1);
    rt.pointer_up(right_target_x, mid_y);
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty(), "live_only warnings: {:?}", rt.live_only_warnings());

    let els_dragged = rt.elements();
    let vp_rect = find_el_by_path_suffix(&els_dragged, "tv-qres-3-0/viewport")
        .expect("qres viewport")
        .rect;
    let thumb_after_drag = find_el_by_key(&els_dragged, "tv-qres-3-0-hscroll-thumb");
    assert!(
        thumb_after_drag.rect[0] > start_thumb_x + 40.0,
        "thumb did not move right: before={start_thumb_x}, after={:?}",
        thumb_after_drag.rect
    );
    let header_c8 = find_el_by_path_suffix(&els_dragged, "/header/c-8").expect("header c-8 visible after drag");
    let row0_c8 = find_el_by_path_suffix(&els_dragged, "/r-0/c-8").expect("row 0 c-8 visible after drag");
    assert!(header_c8.visible[2] > 20.0, "header c-8 not visible: {:?}", header_c8.visible);
    assert!(row0_c8.visible[2] > 20.0, "row0 c-8 not visible: {:?}", row0_c8.visible);
    let frozen_hash = find_el_by_path_suffix(&els_dragged, "/header/#").expect("frozen # header");
    assert!(
        (frozen_hash.rect[0] - vp_rect[0]).abs() <= 1.0,
        "frozen # drifted from left edge: hash={:?} vp={:?}",
        frozen_hash.rect,
        vp_rect
    );

    // 2. Click left end of track to jump back to start.
    rt.pointer_down(track_rect[0] + 6.0, mid_y);
    rt.pointer_up(track_rect[0] + 6.0, mid_y);
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());
    let els_reset = rt.elements();
    let thumb_reset = find_el_by_key(&els_reset, "tv-qres-3-0-hscroll-thumb");
    assert!(
        thumb_reset.rect[0] <= start_thumb_x + 1.5,
        "track jump did not return to start: thumb={:?} start_thumb_x={start_thumb_x}",
        thumb_reset.rect
    );

    // 3. Horizontal wheel scrolls content and moves scrollbar thumb in sync.
    let vx = vp_rect[0] + vp_rect[2] * 0.5;
    let vy = vp_rect[1] + vp_rect[3] * 0.5;
    rt.wheel_xy(vx, vy, -280.0, 0.0);
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());
    let els_wheel = rt.elements();
    let thumb_after_wheel = find_el_by_key(&els_wheel, "tv-qres-3-0-hscroll-thumb").rect[0];
    assert!(
        thumb_after_wheel > track_rect[0] + 10.0,
        "horizontal wheel did not update thumb: {thumb_after_wheel} vs track {}",
        track_rect[0]
    );

    // 4. macOS Shift + precise vertical wheel scrolls horizontally too.
    rt.wheel_event(
        WheelEvent::new(0.0, -240.0)
            .at(vx, vy)
            .mods(Mods {
                shift: true,
                ..Default::default()
            })
            .precise(true),
    );
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());
    let thumb_after_shift_wheel = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    assert!(
        thumb_after_shift_wheel > thumb_after_wheel + 10.0,
        "Shift+precise wheel did not scroll further right: before={thumb_after_wheel}, after={thumb_after_shift_wheel}"
    );

    // 5. Clicking a visible row focuses the grid; Left/Right arrows scroll horizontally without losing row selection.
    let r0 = find_el_by_key(&rt.elements(), "r-0").visible;
    rt.pointer_down(r0[0] + 80.0, r0[1] + r0[3] * 0.5);
    rt.pointer_up(r0[0] + 80.0, r0[1] + r0[3] * 0.5);
    settle_frames(&mut rt);
    assert!(
        find_el_by_key(&rt.elements(), "tv-qres-3-0").focused,
        "clicking visible row should focus results table"
    );
    let before_keys_x = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    rt.key(KeyEvent::from(Key::Left));
    rt.key(KeyEvent::from(Key::Left));
    settle_frames(&mut rt);
    let after_left_x = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    assert!(
        after_left_x < before_keys_x - 10.0,
        "Key::Left did not scroll left: before={before_keys_x}, after={after_left_x}"
    );
    rt.key(KeyEvent {
        key: Key::Right,
        mods: Mods {
            shift: true,
            ..Default::default()
        },
    });
    settle_frames(&mut rt);
    let after_shift_right_x = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    assert!(
        after_shift_right_x > after_left_x + 20.0,
        "Shift+Right did not scroll right: left={after_left_x}, right={after_shift_right_x}"
    );
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .active_query_tab()
            .expect("query tab")
            .active_result_state()
            .expect("res0")
            .selected_row,
        Some(0)
    );
    assert!(rt.live_only_warnings().is_empty());

    // 5b. Dragging the scrollbar preserves table focus so arrow keys keep working immediately.
    let els_before_drag = rt.elements();
    let bar_after = find_el_by_key(&els_before_drag, "tv-qres-3-0-hscroll-bar").rect;
    let track_after = find_el_by_key(&els_before_drag, "tv-qres-3-0-hscroll-track").rect;
    let thumb_after = find_el_by_key(&els_before_drag, "tv-qres-3-0-hscroll-thumb").rect;
    let bar_mid_y = thumb_after[1] + thumb_after[3] * 0.5;
    let drag_end_x = track_after[0] + track_after[2] - 8.0;
    rt.pointer_down(thumb_after[0] + thumb_after[2] * 0.5, bar_mid_y);
    rt.frame(0.0);
    rt.pointer_move(drag_end_x, bar_mid_y);
    rt.frame(0.1);
    rt.pointer_up(drag_end_x, bar_mid_y);
    settle_frames(&mut rt);
    assert!(
        find_el_by_key(&rt.elements(), "tv-qres-3-0").focused,
        "dragging horizontal scrollbar must preserve table focus"
    );
    let thumb_after_re_drag = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    rt.key(KeyEvent::from(Key::Left));
    settle_frames(&mut rt);
    let thumb_after_post_drag_left = find_el_by_key(&rt.elements(), "tv-qres-3-0-hscroll-thumb").rect[0];
    assert!(
        thumb_after_post_drag_left < thumb_after_re_drag - 10.0,
        "Key::Left after scrollbar drag did not scroll: drag={thumb_after_re_drag}, left={thumb_after_post_drag_left}"
    );
    // Clicking the 4-pt left scrollbar gutter also preserves table focus.
    rt.pointer_down(bar_after[0] + 1.5, bar_mid_y);
    rt.pointer_up(bar_after[0] + 1.5, bar_mid_y);
    settle_frames(&mut rt);
    assert!(
        find_el_by_key(&rt.elements(), "tv-qres-3-0").focused,
        "clicking scrollbar 4-pt gutter must preserve table focus"
    );
    assert!(rt.live_only_warnings().is_empty());

    // 6. Narrow Result 2 (`active_students`) omits the horizontal scrollbar.
    let (tab1_x, tab1_y) = rt.find("query-result-tab-1").expect("result tab 1");
    rt.pointer_down(tab1_x, tab1_y);
    rt.pointer_up(tab1_x, tab1_y);
    settle_frames(&mut rt);
    assert!(rt.find("tv-qres-3-1-hscroll-bar").is_none());
    assert!(rt.find("tv-qres-3-1-hscroll-track").is_none());
}

fn wide_greendale_columns() -> Vec<ColumnMeta> {
    let col = |name: &str, data_type: &str, udt_name: &str| ColumnMeta {
        name: name.to_owned(),
        data_type: data_type.to_owned(),
        udt_name: udt_name.to_owned(),
        nullable: false,
        default_expr: None,
        is_primary_key: name == "student_id",
        is_unique: false,
        comment: None,
        foreign_key: None,
    };
    vec![
        col("student_id", "integer", "int4"),
        col("campus_id", "integer", "int4"),
        col("full_name", "text", "text"),
        col("study_room", "text", "text"),
        col("campus_email", "text", "text"),
        col("air_conditioning_tier", "text", "text"),
        col("paintball_rank", "text", "text"),
        col("club_affiliation", "text", "text"),
        col("graduation_term", "text", "text"),
        col("organization_id", "uuid", "uuid"),
    ]
}

#[test]
fn sql_results_single_row_and_zero_row_wide_results_scroll_horizontally() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-editor-dark");
    settle_frames(&mut rt);

    // 1. Single wide row (mirroring 1-row result panel clipping right-side fields).
    let single_row = vec![vec![
        CellValue::Int(1422876),
        CellValue::Int(1413748),
        CellValue::Text("Troy Barnes".to_owned()),
        CellValue::Text("Greendale Community College - Study Room F".to_owned()),
        CellValue::Text("troy.barnes@greendale.edu".to_owned()),
        CellValue::Text("Truest Repairman Annex".to_owned()),
        CellValue::Text("Season 2 Paintball Co-Champion".to_owned()),
        CellValue::Text("Spanish 101 Study Group".to_owned()),
        CellValue::Text("Spring 2014".to_owned()),
        CellValue::Text("00000000-0000-0000-0000-000000000007".to_owned()),
    ]];
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        q.replace_results(vec![QueryStatementResult {
            sql: "SELECT * FROM public.greendale_roster WHERE student_id = 1422876;".to_owned(),
            columns: wide_greendale_columns(),
            rows: single_row,
            rows_affected: None,
            total_rows_seen: 1,
            truncated: false,
            elapsed_ms: 4,
            error: None,
        }]);
    });
    settle_frames(&mut rt);

    let key1 = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab")
        .active_result_state()
        .expect("res1")
        .table_key
        .clone();
    let els = rt.elements();
    let track = find_el_by_key(&els, &format!("{key1}-hscroll-track"));
    let thumb = find_el_by_key(&els, &format!("{key1}-hscroll-thumb"));
    assert!(thumb.rect[2] < track.rect[2]);
    let mid_y = thumb.rect[1] + thumb.rect[3] * 0.5;
    let right_x = track.rect[0] + track.rect[2] - 4.0;
    rt.pointer_down(thumb.rect[0] + thumb.rect[2] * 0.5, mid_y);
    rt.frame(0.0);
    rt.pointer_move(right_x, mid_y);
    rt.frame(0.1);
    rt.pointer_up(right_x, mid_y);
    settle_frames(&mut rt);

    let els_after = rt.elements();
    let last_cell = find_el_by_path_suffix(&els_after, "/r-0/c-9").expect("organization_id cell visible");
    assert!(last_cell.visible[2] > 40.0, "organization_id cell not visible: {:?}", last_cell.visible);
    assert!(rt.live_only_warnings().is_empty());

    // 2. Replacing with a zero-row wide result resets horizontal scroll back to the left edge
    // and still scrolls its header horizontally when dragged/clicked.
    rt.update(|s, _| {
        let q = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        q.replace_results(vec![QueryStatementResult {
            sql: "SELECT * FROM public.greendale_roster WHERE false;".to_owned(),
            columns: wide_greendale_columns(),
            rows: Vec::new(),
            rows_affected: None,
            total_rows_seen: 0,
            truncated: false,
            elapsed_ms: 2,
            error: None,
        }]);
    });
    settle_frames(&mut rt);
    assert!(has_text(&rt, "Query returned 0 rows"));

    let key2 = rt
        .state
        .active_session()
        .expect("session")
        .active_query_tab()
        .expect("query tab")
        .active_result_state()
        .expect("res2")
        .table_key
        .clone();
    assert_ne!(key1, key2);
    let els_zero = rt.elements();
    let bar_zero_rect = find_el_by_key(&els_zero, &format!("{key2}-hscroll-bar")).rect;
    let track_zero = find_el_by_key(&els_zero, &format!("{key2}-hscroll-track"));
    let thumb_zero = find_el_by_key(&els_zero, &format!("{key2}-hscroll-thumb"));
    assert!(thumb_zero.rect[2] < track_zero.rect[2]);
    assert!(
        thumb_zero.rect[0] <= track_zero.rect[0] + 5.5,
        "new result should start scrolled at left edge: thumb={:?} track={:?}",
        thumb_zero.rect,
        track_zero.rect
    );
    let track_zero_rect = track_zero.rect;
    let thumb_zero_start_x = thumb_zero.rect[0];
    let mid_y0 = thumb_zero.rect[1] + thumb_zero.rect[3] * 0.5;
    let vp0 = find_el_by_path_suffix(&els_zero, &format!("{key2}/viewport"))
        .expect("0-row viewport")
        .rect;
    let content0_x = find_el_by_path_suffix(&els_zero, &format!("{key2}/viewport/content"))
        .expect("0-row content")
        .rect[0];

    // Native horizontal wheel moves both the headers and the scrollbar thumb in sync.
    let vx0 = vp0[0] + vp0[2] * 0.5;
    let vy0 = vp0[1] + 50.0;
    rt.wheel_xy(vx0, vy0, -160.0, 0.0);
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_wheel = rt.elements();
    let content_after_wheel_x = find_el_by_path_suffix(&els_zero_wheel, &format!("{key2}/viewport/content"))
        .expect("0-row content after wheel")
        .rect[0];
    assert!(
        (content0_x - content_after_wheel_x - 160.0).abs() <= 2.0,
        "expected ~160pt wheel scroll on 0-row header: before={content0_x}, after={content_after_wheel_x}"
    );
    let thumb_after_wheel_x = find_el_by_key(&els_zero_wheel, &format!("{key2}-hscroll-thumb")).rect[0];
    assert!(
        thumb_after_wheel_x > thumb_zero_start_x + 15.0,
        "0-row horizontal wheel did not update thumb: start={thumb_zero_start_x}, after={thumb_after_wheel_x}"
    );

    // Option-modified horizontal swipe on a zero-row grid must also stay synced with the thumb.
    rt.wheel_event(
        WheelEvent::new(-120.0, 0.0)
            .at(vx0, vy0)
            .mods(Mods { alt: true, ..Mods::default() })
            .precise(true),
    );
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_alt_wheel = rt.elements();
    let content_after_alt_wheel_x =
        find_el_by_path_suffix(&els_zero_alt_wheel, &format!("{key2}/viewport/content"))
            .expect("0-row content after alt wheel")
            .rect[0];
    assert!(
        (content_after_wheel_x - content_after_alt_wheel_x - 120.0).abs() <= 2.0,
        "expected ~120pt Option+wheel scroll on 0-row header: wheel={content_after_wheel_x}, alt_wheel={content_after_alt_wheel_x}"
    );
    let thumb_after_alt_wheel_x =
        find_el_by_key(&els_zero_alt_wheel, &format!("{key2}-hscroll-thumb")).rect[0];
    assert!(
        thumb_after_alt_wheel_x > thumb_after_wheel_x + 10.0,
        "0-row Option+horizontal wheel did not update thumb: wheel={thumb_after_wheel_x}, alt_wheel={thumb_after_alt_wheel_x}"
    );

    // Phased horizontal swipe whose middle delta wobbles vertically must keep latching horizontally.
    rt.wheel_event(
        WheelEvent::new(-90.0, -10.0)
            .at(vx0, vy0)
            .phase(GesturePhase::Began)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(-15.0, -20.0)
            .at(vx0, vy0)
            .phase(GesturePhase::Moved)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(-120.0, 0.0)
            .at(vx0, vy0)
            .phase(GesturePhase::Moved)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(0.0, 0.0)
            .at(vx0, vy0)
            .phase(GesturePhase::Ended)
            .precise(true),
    );
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_phased = rt.elements();
    let content_after_phased_x =
        find_el_by_path_suffix(&els_zero_phased, &format!("{key2}/viewport/content"))
            .expect("0-row content after phased swipe")
            .rect[0];
    assert!(
        (content_after_alt_wheel_x - content_after_phased_x - 225.0).abs() <= 2.0,
        "phased swipe with vertical wobble stalled on 0-row grid: before={content_after_alt_wheel_x}, after={content_after_phased_x}"
    );
    let thumb_after_phased_x =
        find_el_by_key(&els_zero_phased, &format!("{key2}-hscroll-thumb")).rect[0];
    assert!(
        thumb_after_phased_x > thumb_after_alt_wheel_x + 15.0,
        "0-row phased horizontal swipe did not update thumb: before={thumb_after_alt_wheel_x}, after={thumb_after_phased_x}"
    );

    // Clicking the zero-row viewport focuses the grid and Key::Right advances forward from wheel offset.
    rt.pointer_down(vx0, vy0);
    rt.pointer_up(vx0, vy0);
    settle_frames(&mut rt);
    assert!(
        find_el_by_key(&rt.elements(), &key2).focused,
        "clicking 0-row grid should focus table"
    );
    rt.key(KeyEvent::from(Key::Right));
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_key = rt.elements();
    let content_after_key_x = find_el_by_path_suffix(&els_zero_key, &format!("{key2}/viewport/content"))
        .expect("0-row content after key")
        .rect[0];
    assert!(
        content_after_key_x < content_after_phased_x - 80.0,
        "Key::Right on 0-row grid jumped backward or stalled: phased={content_after_phased_x}, key={content_after_key_x}"
    );
    let thumb_after_key_x = find_el_by_key(&els_zero_key, &format!("{key2}-hscroll-thumb")).rect[0];
    assert!(
        thumb_after_key_x > thumb_after_phased_x + 10.0,
        "Key::Right did not advance 0-row thumb: phased={thumb_after_phased_x}, key={thumb_after_key_x}"
    );

    // Click the 4-pt left gutter to return to x=0 while preserving table focus, then verify
    // unphased diagonal swipe latching and Shift+precise momentum conversion on 0-row grids.
    rt.pointer_down(bar_zero_rect[0] + 1.5, mid_y0);
    rt.pointer_up(bar_zero_rect[0] + 1.5, mid_y0);
    settle_frames(&mut rt);
    let els_zero_gutter = rt.elements();
    assert!(
        find_el_by_key(&els_zero_gutter, &key2).focused,
        "clicking 0-row scrollbar 4-pt gutter must preserve table focus"
    );
    let content_at_start_x =
        find_el_by_path_suffix(&els_zero_gutter, &format!("{key2}/viewport/content"))
            .expect("0-row content at start")
            .rect[0];
    assert!(
        (content_at_start_x - content0_x).abs() <= 1.5,
        "gutter click should reset scroll to start: expected={content0_x}, got={content_at_start_x}"
    );

    // Unphased precise horizontal swipe whose middle delta is vertically dominant must stay latched.
    rt.wheel_event(
        WheelEvent::new(-90.0, -10.0)
            .at(vx0, vy0)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(-15.0, -20.0)
            .at(vx0, vy0)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(-120.0, 0.0)
            .at(vx0, vy0)
            .precise(true),
    );
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_unphased = rt.elements();
    let content_after_unphased_x =
        find_el_by_path_suffix(&els_zero_unphased, &format!("{key2}/viewport/content"))
            .expect("0-row content after unphased swipe")
            .rect[0];
    assert!(
        (content0_x - content_after_unphased_x - 225.0).abs() <= 2.0,
        "unphased swipe with vertical wobble stalled on 0-row grid: start={content0_x}, after={content_after_unphased_x}"
    );

    // Wait past UNPHASED_WHEEL_IDLE (0.1s), then send a Shift+precise swipe followed by momentum
    // within MOMENTUM_GAP (<= 0.05s) so the first momentum Began(dy) delta is converted too.
    rt.frame(0.15);
    let shift = Mods {
        shift: true,
        ..Mods::default()
    };
    rt.wheel_event(
        WheelEvent::new(0.0, -90.0)
            .at(vx0, vy0)
            .mods(shift)
            .phase(GesturePhase::Began)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(0.0, -15.0)
            .at(vx0, vy0)
            .mods(shift)
            .phase(GesturePhase::Moved)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(0.0, 0.0)
            .at(vx0, vy0)
            .mods(shift)
            .phase(GesturePhase::Ended)
            .precise(true),
    );
    rt.frame(0.016);
    rt.wheel_event(
        WheelEvent::new(0.0, -120.0)
            .at(vx0, vy0)
            .mods(shift)
            .phase(GesturePhase::Began)
            .precise(true),
    );
    rt.wheel_event(
        WheelEvent::new(0.0, 0.0)
            .at(vx0, vy0)
            .mods(shift)
            .phase(GesturePhase::Ended)
            .precise(true),
    );
    settle_frames(&mut rt);
    assert!(rt.live_only_warnings().is_empty());

    let els_zero_shift_mom = rt.elements();
    let content_after_shift_mom_x =
        find_el_by_path_suffix(&els_zero_shift_mom, &format!("{key2}/viewport/content"))
            .expect("0-row content after shift momentum")
            .rect[0];
    assert!(
        (content_after_unphased_x - content_after_shift_mom_x - 225.0).abs() <= 2.0,
        "Shift+precise momentum dropped delta on 0-row grid: before={content_after_unphased_x}, after={content_after_shift_mom_x}"
    );

    rt.pointer_down(track_zero_rect[0] + track_zero_rect[2] - 4.0, mid_y0);
    rt.pointer_up(track_zero_rect[0] + track_zero_rect[2] - 4.0, mid_y0);
    settle_frames(&mut rt);
    let els_zero_after = rt.elements();
    assert!(
        find_el_by_key(&els_zero_after, &key2).focused,
        "clicking 0-row scrollbar track must preserve table focus"
    );
    let header_c9 = find_el_by_path_suffix(&els_zero_after, "/header/c-9").expect("0-row header c-9");
    assert!(header_c9.visible[2] > 40.0, "0-row header c-9 not visible: {:?}", header_c9.visible);
    assert!(rt.live_only_warnings().is_empty());
}

#[test]
fn table_data_grid_horizontal_scrollbar_scrolls_wide_columns() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    settle_frames(&mut rt);

    let els = rt.elements();
    let track = find_el_by_key(&els, "tv-data-1-hscroll-track");
    let thumb = find_el_by_key(&els, "tv-data-1-hscroll-thumb");
    assert!(thumb.rect[2] >= 32.0 && thumb.rect[2] < track.rect[2]);

    let mid_y = thumb.rect[1] + thumb.rect[3] * 0.5;
    let right_x = track.rect[0] + track.rect[2] - 6.0;
    rt.pointer_down(thumb.rect[0] + thumb.rect[2] * 0.5, mid_y);
    rt.frame(0.0);
    rt.pointer_move(right_x, mid_y);
    rt.frame(0.1);
    rt.pointer_up(right_x, mid_y);
    settle_frames(&mut rt);

    let els_after = rt.elements();
    let group_cell = find_el_by_path_suffix(&els_after, "/r-0/group_id").expect("group_id visible after drag");
    assert!(group_cell.visible[2] > 10.0, "group_id cell not visible: {:?}", group_cell.visible);
    assert!(rt.live_only_warnings().is_empty());
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
    engine: DbEngine,
    server_info: ServerInfo,
    mysql_quote_mode: Mutex<Option<MysqlQuoteMode>>,
    next_lease_mysql_mode: Mutex<Option<Option<MysqlQuoteMode>>>,
    postgres_session_mode: Mutex<Option<PostgresSessionMode>>,
    next_lease_pg_mode: Mutex<Option<Option<PostgresSessionMode>>>,
    disconnected: AtomicBool,
    half_open: AtomicBool,
    fail_next_sql: AtomicBool,
    fail_next_page: AtomicBool,
    fail_next_pre_dispatch: AtomicBool,
    fail_with_auth_on_disconnect: AtomicBool,
    ping_delay: Mutex<Duration>,
    pre_dispatch_delay: Mutex<Duration>,
    sql_delay_once: Mutex<Option<Duration>>,
    page_delay_once: Mutex<Option<Duration>>,
    objects_delay_once: Mutex<Option<Duration>>,
    columns_delay_once: Mutex<Option<Duration>>,
    columns_override: Mutex<Option<Vec<ColumnSummary>>>,
    cancelled_keys: Mutex<Vec<u64>>,
    executed_batches: Mutex<Vec<Vec<String>>>,
    ping_calls: AtomicUsize,
    page_calls: AtomicUsize,
    objects_calls: AtomicUsize,
    describe_calls: AtomicUsize,
    sql_calls: AtomicUsize,
    pre_dispatch_entries: AtomicUsize,
    explain_calls: AtomicUsize,
    tx_calls: AtomicUsize,
}

impl MockSession {
    fn with_engine(engine: DbEngine, database: &str, mysql_quote_mode: Option<MysqlQuoteMode>) -> Self {
        Self {
            engine,
            server_info: ServerInfo {
                version: match engine {
                    DbEngine::Postgres => "PostgreSQL 18.4".to_owned(),
                    DbEngine::Mysql => "MySQL 8.4.11".to_owned(),
                    DbEngine::Sqlite => "SQLite 3.49.1".to_owned(),
                },
                current_database: database.to_owned(),
                current_user: "troy_barnes".to_owned(),
            },
            mysql_quote_mode: Mutex::new(mysql_quote_mode),
            next_lease_mysql_mode: Mutex::new(None),
            postgres_session_mode: Mutex::new(
                (engine == DbEngine::Postgres).then(|| PostgresSessionMode::new(true)),
            ),
            next_lease_pg_mode: Mutex::new(None),
            disconnected: AtomicBool::new(false),
            half_open: AtomicBool::new(false),
            fail_next_sql: AtomicBool::new(false),
            fail_next_page: AtomicBool::new(false),
            fail_next_pre_dispatch: AtomicBool::new(false),
            fail_with_auth_on_disconnect: AtomicBool::new(false),
            ping_delay: Mutex::new(Duration::ZERO),
            pre_dispatch_delay: Mutex::new(Duration::ZERO),
            sql_delay_once: Mutex::new(None),
            page_delay_once: Mutex::new(None),
            objects_delay_once: Mutex::new(None),
            columns_delay_once: Mutex::new(None),
            columns_override: Mutex::new(None),
            cancelled_keys: Mutex::new(Vec::new()),
            executed_batches: Mutex::new(Vec::new()),
            ping_calls: AtomicUsize::new(0),
            page_calls: AtomicUsize::new(0),
            objects_calls: AtomicUsize::new(0),
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

    fn set_fail_next_page(&self, value: bool) {
        self.fail_next_page.store(value, Ordering::Release);
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

    fn set_sql_delay_once(&self, delay: Duration) {
        *self.sql_delay_once.lock().expect("sql_delay_once lock") = Some(delay);
    }

    fn set_next_lease_mysql_mode(&self, mode: Option<MysqlQuoteMode>) {
        *self
            .next_lease_mysql_mode
            .lock()
            .expect("next_lease_mysql_mode lock") = Some(mode);
    }

    fn apply_lease_mysql_mode_override(&self) {
        if let Some(new_mode) = self
            .next_lease_mysql_mode
            .lock()
            .expect("next_lease_mysql_mode lock")
            .take()
        {
            *self.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = new_mode;
        }
    }

    fn normalize_pg_savepoint_ident(
        raw: &str,
        max_ident_bytes: usize,
        server_encoding: Option<tv_core::PostgresServerEncoding>,
    ) -> String {
        let limit = max_ident_bytes.max(1);
        let norm = match raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            Some(quoted) => quoted.to_owned(),
            None => raw.to_ascii_lowercase(),
        };
        if norm.len() >= limit && norm.as_bytes()[..limit].is_ascii() {
            return norm[..limit].to_owned();
        }
        if norm.is_ascii() || server_encoding != Some(tv_core::PostgresServerEncoding::SingleByte) {
            let mut end = limit.min(norm.len());
            while end > 0 && !norm.is_char_boundary(end) {
                end -= 1;
            }
            norm[..end].to_owned()
        } else {
            norm.chars().take(limit).collect()
        }
    }

    fn parse_pg_rollback_to(
        clean_stmt: &str,
        max_ident_bytes: usize,
        server_encoding: Option<tv_core::PostgresServerEncoding>,
    ) -> Option<String> {
        let mut parts = clean_stmt.split_whitespace();
        let first = parts.next()?;
        if !first.eq_ignore_ascii_case("ROLLBACK") {
            return None;
        }
        let mut next = parts.next()?;
        if next.eq_ignore_ascii_case("WORK") || next.eq_ignore_ascii_case("TRANSACTION") {
            next = parts.next()?;
        }
        if !next.eq_ignore_ascii_case("TO") {
            return None;
        }
        let mut target = parts.next()?;
        if target.eq_ignore_ascii_case("SAVEPOINT") {
            target = parts.next()?;
        }
        Some(Self::normalize_pg_savepoint_ident(
            target,
            max_ident_bytes,
            server_encoding,
        ))
    }

    fn parse_pg_scs_mock_mutation(stmt_sql: &str) -> Option<(bool, bool)> {
        let upper = stmt_sql.to_ascii_uppercase();
        if upper.contains("CASE ") {
            return None;
        }
        let parse_val = |raw: &str| -> Option<bool> {
            let clean = raw
                .trim()
                .trim_end_matches([',', ')', ';'])
                .trim()
                .trim_matches('\'')
                .to_ascii_uppercase();
            match clean.as_str() {
                "OFF" | "0" | "FALSE" => Some(false),
                "ON" | "1" | "TRUE" => Some(true),
                _ => None,
            }
        };
        if let Some(pos) = upper.find("SET_CONFIG")
            && let Some(open) = upper[pos..].find('(')
            && let Some(close) = upper[pos + open + 1..].find(')')
        {
            let args: Vec<&str> = upper[pos + open + 1..pos + open + 1 + close]
                .split(',')
                .collect();
            if args.len() == 3
                && args[0].trim().trim_matches('\'') == "STANDARD_CONFORMING_STRINGS"
            {
                let val = parse_val(args[1])?;
                let is_local = parse_val(args[2])?;
                return Some((is_local, val));
            }
        }
        if let Some(pos) = upper.find("STANDARD_CONFORMING_STRINGS") {
            let is_local = upper[..pos].contains("LOCAL");
            let mut rest = upper[pos + "STANDARD_CONFORMING_STRINGS".len()..].trim_start();
            if let Some(after_eq) = rest.strip_prefix('=') {
                rest = after_eq.trim_start();
            } else {
                rest = rest.strip_prefix("TO")?.trim_start();
            }
            let val = parse_val(rest.split_whitespace().next().unwrap_or(rest))?;
            return Some((is_local, val));
        }
        None
    }

    fn set_postgres_session_mode(&self, val: Option<PostgresSessionMode>) {
        *self
            .postgres_session_mode
            .lock()
            .expect("postgres_session_mode lock") = val;
    }

    fn set_postgres_standard_conforming_strings(&self, val: Option<bool>) {
        let mut guard = self
            .postgres_session_mode
            .lock()
            .expect("postgres_session_mode lock");
        *guard = val.map(|b| match guard.clone() {
            Some(prev) => {
                let is_idle = prev.tx_state == Some(tv_core::PostgresTxState::Idle);
                PostgresSessionMode {
                    standard_conforming_strings: b,
                    session_standard_conforming_strings: Some(b),
                    pre_tx_standard_conforming_strings: if is_idle {
                        Some(b)
                    } else {
                        prev.pre_tx_standard_conforming_strings
                    },
                    ..prev
                }
            }
            None => PostgresSessionMode::new(b),
        });
    }

    fn set_next_lease_pg_scs(&self, val: Option<bool>) {
        let current = self
            .postgres_session_mode
            .lock()
            .expect("postgres_session_mode lock")
            .clone();
        let next = val.map(|b| match current {
            Some(prev) => {
                let is_idle = prev.tx_state == Some(tv_core::PostgresTxState::Idle);
                PostgresSessionMode {
                    standard_conforming_strings: b,
                    session_standard_conforming_strings: Some(b),
                    pre_tx_standard_conforming_strings: if is_idle {
                        Some(b)
                    } else {
                        prev.pre_tx_standard_conforming_strings
                    },
                    ..prev
                }
            }
            None => PostgresSessionMode::new(b),
        });
        *self
            .next_lease_pg_mode
            .lock()
            .expect("next_lease_pg_mode lock") = Some(next);
    }

    fn apply_lease_pg_scs_override(&self) {
        if let Some(new_mode) = self
            .next_lease_pg_mode
            .lock()
            .expect("next_lease_pg_mode lock")
            .take()
        {
            *self
                .postgres_session_mode
                .lock()
                .expect("postgres_session_mode lock") = new_mode;
        }
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
        self.engine
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    fn mysql_quote_mode(&self) -> Option<MysqlQuoteMode> {
        *self.mysql_quote_mode.lock().expect("mysql_quote_mode lock")
    }

    fn postgres_session_mode(&self) -> Option<PostgresSessionMode> {
        self.postgres_session_mode
            .lock()
            .expect("postgres_session_mode lock")
            .clone()
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
            let delay = self
                .objects_delay_once
                .lock()
                .expect("objects_delay_once lock")
                .take();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            self.objects_calls.fetch_add(1, Ordering::AcqRel);
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
            let delay = self
                .columns_delay_once
                .lock()
                .expect("columns_delay_once lock")
                .take();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let cols = if let Some(custom) =
                self.columns_override.lock().expect("columns_override lock").clone()
            {
                custom
            } else {
                mock_columns_for(object)
                    .into_iter()
                    .map(|c| ColumnSummary {
                        name: c.name,
                        data_type: c.data_type,
                        nullable: c.nullable,
                        is_primary_key: c.is_primary_key,
                    })
                    .collect()
            };
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
            if self.fail_next_page.swap(false, Ordering::AcqRel) {
                if self.engine == DbEngine::Postgres
                    && let Some(ref mut mode) = *self
                        .postgres_session_mode
                        .lock()
                        .expect("postgres_session_mode lock")
                    && mode.tx_state == Some(tv_core::PostgresTxState::InTransaction)
                {
                    mode.tx_state = Some(tv_core::PostgresTxState::Aborted);
                }
                return Err(DbError::InvalidInput("ERROR: division by zero".to_owned()));
            }
            let delay = self.page_delay_once.lock().expect("page_delay_once lock").take();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
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
            self.executed_batches
                .lock()
                .expect("executed_batches lock")
                .push(statements.to_vec());
            let mut results = Vec::new();
            let mut logs = Vec::new();
            for sql in statements {
                if sql.contains("/*UNKNOWN_MODE*/") {
                    *self.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = None;
                    return Err(DbError::Cancelled);
                }
                let deferred_commit_fail = sql.contains("/*DEFERRED_COMMIT_FAIL*/");
                let sql_stripped = if deferred_commit_fail {
                    sql.replace("/*DEFERRED_COMMIT_FAIL*/", "")
                } else {
                    sql.clone()
                };
                let clean_raw = sql_stripped.trim_end_matches(';').trim();
                let upper = sql_stripped.to_ascii_uppercase();
                let mut stmt_error: Option<String> = None;
                if self.engine == DbEngine::Mysql
                    && upper.contains("SQL_MODE")
                    && (upper.contains("GLOBAL") || upper.contains("PERSIST"))
                {
                    let new_mode = if upper.contains("MSSQL") && upper.contains("NO_BACKSLASH_ESCAPES") {
                        MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH
                    } else if upper.contains("MSSQL") {
                        MysqlQuoteMode::MSSQL
                    } else if upper.contains("NO_BACKSLASH_ESCAPES") {
                        MysqlQuoteMode::NO_BACKSLASH_ESCAPES
                    } else if upper.contains("ANSI_QUOTES") {
                        MysqlQuoteMode::ANSI_QUOTES
                    } else {
                        MysqlQuoteMode::DEFAULT
                    };
                    *self.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = Some(new_mode);
                }
                if self.engine == DbEngine::Postgres {
                    let mut guard = self
                        .postgres_session_mode
                        .lock()
                        .expect("postgres_session_mode lock");
                    if let Some(ref mut mode) = *guard {
                        let clean_upper = upper.trim_end_matches(';').trim();
                        let max_ident_bytes = mode.max_identifier_length;
                        if mode.tx_state == Some(tv_core::PostgresTxState::Aborted) {
                            if clean_upper == "COMMIT"
                                || clean_upper == "END"
                                || clean_upper == "ROLLBACK"
                                || clean_upper == "ABORT"
                            {
                                mode.tx_state = Some(tv_core::PostgresTxState::Idle);
                                mode.session_standard_conforming_strings =
                                    mode.pre_tx_standard_conforming_strings;
                                if let Some(pre) = mode.pre_tx_standard_conforming_strings {
                                    mode.standard_conforming_strings = pre;
                                }
                                mode.savepoints.clear();
                                mode.has_untracked_savepoints = false;
                                if deferred_commit_fail {
                                    stmt_error = Some(
                                        "ERROR: duplicate key value violates unique constraint (deferred)"
                                            .to_owned(),
                                    );
                                }
                            } else if let Some(sp_name) =
                                Self::parse_pg_rollback_to(
                                clean_raw,
                                max_ident_bytes,
                                mode.server_encoding,
                            )
                                && let Some(pos) =
                                    mode.savepoints.iter().rposition(|sp| sp.name == sp_name)
                            {
                                let saved = mode.savepoints[pos].clone();
                                if let Some(active) = saved.active_standard_conforming_strings {
                                    mode.standard_conforming_strings = active;
                                }
                                mode.session_standard_conforming_strings =
                                    saved.session_standard_conforming_strings;
                                mode.savepoints.truncate(pos + 1);
                                mode.tx_state = Some(tv_core::PostgresTxState::InTransaction);
                            } else {
                                stmt_error = Some(
                                    "ERROR: current transaction is aborted, commands ignored until end of transaction block"
                                        .to_owned(),
                                );
                            }
                        } else if clean_upper == "BEGIN"
                            || clean_upper.starts_with("START TRANSACTION")
                        {
                            if mode.tx_state == Some(tv_core::PostgresTxState::Idle) {
                                mode.tx_state = Some(tv_core::PostgresTxState::InTransaction);
                                mode.pre_tx_standard_conforming_strings =
                                    mode.session_standard_conforming_strings;
                                mode.savepoints.clear();
                                mode.has_untracked_savepoints = false;
                            }
                        } else if clean_upper == "COMMIT" || clean_upper == "END" {
                            if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction) {
                                mode.tx_state = Some(tv_core::PostgresTxState::Idle);
                                if deferred_commit_fail {
                                    mode.session_standard_conforming_strings =
                                        mode.pre_tx_standard_conforming_strings;
                                    if let Some(pre) = mode.pre_tx_standard_conforming_strings {
                                        mode.standard_conforming_strings = pre;
                                    }
                                    stmt_error = Some(
                                        "ERROR: duplicate key value violates unique constraint (deferred)"
                                            .to_owned(),
                                    );
                                } else {
                                    if let Some(sess) = mode.session_standard_conforming_strings {
                                        mode.standard_conforming_strings = sess;
                                    }
                                    mode.pre_tx_standard_conforming_strings =
                                        mode.session_standard_conforming_strings;
                                }
                                mode.savepoints.clear();
                                mode.has_untracked_savepoints = false;
                            }
                        } else if clean_upper == "ROLLBACK" || clean_upper == "ABORT" {
                            if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction) {
                                mode.tx_state = Some(tv_core::PostgresTxState::Idle);
                                mode.session_standard_conforming_strings =
                                    mode.pre_tx_standard_conforming_strings;
                                if let Some(pre) = mode.pre_tx_standard_conforming_strings {
                                    mode.standard_conforming_strings = pre;
                                }
                                mode.savepoints.clear();
                                mode.has_untracked_savepoints = false;
                            }
                        } else if clean_upper.starts_with("SAVEPOINT ") {
                            let sp_raw = clean_raw
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or_default();
                            if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction)
                                && !sp_raw.is_empty()
                            {
                                mode.savepoints.push(tv_core::PostgresSavepointMode {
                                    name: Self::normalize_pg_savepoint_ident(
                                        sp_raw,
                                        max_ident_bytes,
                                        mode.server_encoding,
                                    ),
                                    active_standard_conforming_strings: Some(
                                        mode.standard_conforming_strings,
                                    ),
                                    session_standard_conforming_strings: mode
                                        .session_standard_conforming_strings,
                                });
                            }
                        } else if clean_upper.starts_with("RELEASE ") {
                            let mut parts = clean_raw.split_whitespace();
                            let _ = parts.next();
                            let sp_raw = match parts.next() {
                                Some(w) if w.eq_ignore_ascii_case("SAVEPOINT") => parts.next(),
                                other => other,
                            };
                            if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction)
                                && let Some(sp_raw) = sp_raw
                            {
                                let name =
                                    Self::normalize_pg_savepoint_ident(
                                        sp_raw,
                                        max_ident_bytes,
                                        mode.server_encoding,
                                    );
                                if let Some(pos) =
                                    mode.savepoints.iter().rposition(|sp| sp.name == name)
                                {
                                    mode.savepoints.truncate(pos);
                                } else {
                                    mode.tx_state = Some(tv_core::PostgresTxState::Aborted);
                                    stmt_error =
                                        Some(format!("ERROR: savepoint \"{name}\" does not exist"));
                                }
                            }
                        } else if let Some(sp_name) =
                            Self::parse_pg_rollback_to(
                                clean_raw,
                                max_ident_bytes,
                                mode.server_encoding,
                            )
                        {
                            if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction) {
                                if let Some(pos) =
                                    mode.savepoints.iter().rposition(|sp| sp.name == sp_name)
                                {
                                    let saved = mode.savepoints[pos].clone();
                                    if let Some(active) = saved.active_standard_conforming_strings {
                                        mode.standard_conforming_strings = active;
                                    }
                                    mode.session_standard_conforming_strings =
                                        saved.session_standard_conforming_strings;
                                    mode.savepoints.truncate(pos + 1);
                                } else {
                                    mode.tx_state = Some(tv_core::PostgresTxState::Aborted);
                                    stmt_error = Some(format!(
                                        "ERROR: savepoint \"{sp_name}\" does not exist"
                                    ));
                                }
                            }
                        } else if clean_upper.starts_with("RESET ")
                            && clean_upper.contains("STANDARD_CONFORMING_STRINGS")
                        {
                            let restored = mode.reset_standard_conforming_strings.unwrap_or(true);
                            mode.standard_conforming_strings = restored;
                            mode.session_standard_conforming_strings = Some(restored);
                            if mode.tx_state == Some(tv_core::PostgresTxState::Idle) {
                                mode.pre_tx_standard_conforming_strings = Some(restored);
                            }
                        } else if let Some((is_local, v)) = Self::parse_pg_scs_mock_mutation(sql) {
                            if is_local {
                                if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction) {
                                    mode.standard_conforming_strings = v;
                                }
                            } else {
                                mode.standard_conforming_strings = v;
                                mode.session_standard_conforming_strings = Some(v);
                                if mode.tx_state == Some(tv_core::PostgresTxState::Idle) {
                                    mode.pre_tx_standard_conforming_strings = Some(v);
                                }
                            }
                        } else if clean_upper.contains("1/0")
                            && mode.tx_state == Some(tv_core::PostgresTxState::InTransaction)
                        {
                            mode.tx_state = Some(tv_core::PostgresTxState::Aborted);
                            stmt_error = Some("ERROR: division by zero".to_owned());
                        }
                    }
                }
                if let Some(err_msg) = stmt_error {
                    results.push(QueryStatementResult {
                        sql: sql.clone(),
                        columns: Vec::new(),
                        rows: Vec::new(),
                        rows_affected: None,
                        truncated: false,
                        total_rows_seen: 0,
                        elapsed_ms: 1,
                        error: Some(err_msg.clone()),
                    });
                    logs.push(ExecutedSql {
                        sql: sql.clone(),
                        duration_ms: 1,
                        error: Some(err_msg),
                    });
                    break;
                }
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
            let delay = self.sql_delay_once.lock().expect("sql_delay_once lock").take();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
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

    fn execute_sql_cancellable_checked<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        statements: &'life1 [String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&'life2 AtomicBool>,
        expected_sql_mode: SqlSessionMode,
    ) -> BoxFuture<'async_trait, Result<(QueryBatchResult, Vec<ExecutedSql>), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.apply_lease_mysql_mode_override();
            self.apply_lease_pg_scs_override();
            match self.engine {
                DbEngine::Mysql => {
                    let live_mode = self.mysql_quote_mode();
                    if live_mode != expected_sql_mode.mysql {
                        return Err(DbError::MysqlModeMismatch);
                    }
                    if let Err(msg) = tv_core::validate_mysql_single_statement_batch(statements, live_mode) {
                        return Err(DbError::InvalidInput(msg));
                    }
                }
                DbEngine::Postgres => {
                    if let Some(ref expected_pg) = expected_sql_mode.postgres {
                        if self.postgres_session_mode().as_ref() != Some(expected_pg) {
                            return Err(DbError::PostgresStringModeMismatch);
                        }
                        if let Some(first_stmt) = statements
                            .iter()
                            .map(|s| tv_core::trim_sql_whitespace_for_engine(DbEngine::Postgres, s))
                            .find(|s| !s.is_empty())
                            && tv_core::postgres_slice_has_multiple_statements(
                                first_stmt,
                                expected_pg.standard_conforming_strings,
                            )
                        {
                            return Err(DbError::InvalidInput(
                                "PostgreSQL statement contains multiple semicolon-separated statements; split statements before executing"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                DbEngine::Sqlite => {}
            }
            self.execute_sql_cancellable(statements, row_limit, cancel_key, cancel_flag)
                .await
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
            if self.engine == DbEngine::Postgres
                && let Some(ref mut mode) = *self
                    .postgres_session_mode
                    .lock()
                    .expect("postgres_session_mode lock")
            {
                mode.tx_state = Some(tv_core::PostgresTxState::Idle);
                if let Some(sess) = mode.session_standard_conforming_strings {
                    mode.standard_conforming_strings = sess;
                }
                mode.pre_tx_standard_conforming_strings =
                    mode.session_standard_conforming_strings;
                mode.savepoints.clear();
                mode.has_untracked_savepoints = false;
            }
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
        analyze: bool,
    ) -> BoxFuture<'async_trait, Result<(ExplainPlan, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.explain_calls.fetch_add(1, Ordering::AcqRel);
            self.check_transport()?;
            if self.engine == DbEngine::Postgres {
                if analyze {
                    let mut guard = self
                        .postgres_session_mode
                        .lock()
                        .expect("postgres_session_mode lock");
                    if let Some(ref mut mode) = *guard {
                        if mode.tx_state == Some(tv_core::PostgresTxState::Aborted) {
                            return Err(DbError::InvalidInput(
                                "current transaction is aborted, commands ignored until end of transaction block"
                                    .to_owned(),
                            ));
                        }
                        if let Some((is_local, v)) = Self::parse_pg_scs_mock_mutation(statement) {
                            if is_local {
                                if mode.tx_state == Some(tv_core::PostgresTxState::InTransaction) {
                                    mode.standard_conforming_strings = v;
                                }
                            } else {
                                mode.standard_conforming_strings = v;
                                mode.session_standard_conforming_strings = Some(v);
                                if mode.tx_state == Some(tv_core::PostgresTxState::Idle) {
                                    mode.pre_tx_standard_conforming_strings = Some(v);
                                }
                            }
                        }
                    }
                }
                let plan = ExplainPlan::parse_json(
                    statement.to_owned(),
                    analyze,
                    r#"[{"Plan": {"Node Type": "Result", "Startup Cost": 0.0, "Total Cost": 0.01, "Plan Rows": 1, "Plan Width": 4}}]"#,
                )
                .map_err(|e| DbError::InvalidInput(e.to_string()))?;
                return Ok((
                    plan,
                    ExecutedSql {
                        sql: tv_core::build_explain_sql(statement, analyze),
                        duration_ms: 1,
                        error: None,
                    },
                ));
            }
            Err(DbError::InvalidInput(format!("unused mock explain for {statement}")))
        })
    }

    fn explain_sql_checked<'life0, 'life1, 'async_trait>(
        &'life0 self,
        statement: &'life1 str,
        analyze: bool,
        expected_sql_mode: SqlSessionMode,
    ) -> BoxFuture<'async_trait, Result<(ExplainPlan, ExecutedSql), DbError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.apply_lease_mysql_mode_override();
            self.apply_lease_pg_scs_override();
            match self.engine {
                DbEngine::Mysql => {
                    let live_mode = self.mysql_quote_mode();
                    if live_mode != expected_sql_mode.mysql {
                        return Err(DbError::MysqlModeMismatch);
                    }
                    if tv_core::mysql_slice_has_multiple_statements(
                        tv_core::trim_sql_whitespace_for_engine(DbEngine::Mysql, statement),
                        live_mode,
                    ) {
                        return Err(DbError::InvalidInput(
                            "cannot EXPLAIN a MySQL slice containing multiple statements; select or split a single statement"
                                .to_owned(),
                        ));
                    }
                }
                DbEngine::Postgres => {
                    if let Some(ref expected_pg) = expected_sql_mode.postgres {
                        if self.postgres_session_mode().as_ref() != Some(expected_pg) {
                            return Err(DbError::PostgresStringModeMismatch);
                        }
                        if tv_core::postgres_slice_has_multiple_statements(
                            tv_core::trim_sql_whitespace_for_engine(DbEngine::Postgres, statement),
                            expected_pg.standard_conforming_strings,
                        ) {
                            return Err(DbError::InvalidInput(
                                "cannot EXPLAIN a PostgreSQL slice containing multiple statements; select or split a single statement"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                DbEngine::Sqlite => {}
            }
            self.explain_sql(statement, analyze).await
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
    connect_hosts: Mutex<Vec<String>>,
    fail_connect: Mutex<Option<DbError>>,
    expected_password: Mutex<Option<String>>,
    mysql_quote_mode: Mutex<Option<MysqlQuoteMode>>,
    sessions: Mutex<Vec<Arc<MockSession>>>,
    connect_delay: Duration,
}

impl MockCluster {
    fn new(connect_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            connect_calls: Mutex::new(Vec::new()),
            connect_hosts: Mutex::new(Vec::new()),
            fail_connect: Mutex::new(None),
            expected_password: Mutex::new(None),
            mysql_quote_mode: Mutex::new(None),
            sessions: Mutex::new(Vec::new()),
            connect_delay,
        })
    }

    fn connector(self: &Arc<Self>) -> SessionConnector {
        let cluster = Arc::clone(self);
        Arc::new(move |profile, password, database| {
            let cluster = Arc::clone(&cluster);
            let engine = profile.engine;
            let host = profile.host.clone();
            Box::pin(async move {
                if !cluster.connect_delay.is_zero() {
                    tokio::time::sleep(cluster.connect_delay).await;
                }
                cluster
                    .connect_calls
                    .lock()
                    .expect("connect_calls lock")
                    .push((password.clone(), database.clone()));
                cluster
                    .connect_hosts
                    .lock()
                    .expect("connect_hosts lock")
                    .push(host);

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
                let mysql_mode = *cluster
                    .mysql_quote_mode
                    .lock()
                    .expect("mysql_quote_mode lock");
                let sess = Arc::new(MockSession::with_engine(engine, &db_name, mysql_mode));
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

    fn connect_hosts(&self) -> Vec<String> {
        self.connect_hosts.lock().expect("connect_hosts lock").clone()
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

fn element_rect(rt: &Runtime<AppState>, key: &str) -> [f32; 4] {
    rt.elements()
        .into_iter()
        .find(|el| el.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("missing element {key}"))
        .rect
}

fn visible_console_sql(rt: &Runtime<AppState>) -> (usize, [f32; 4]) {
    let scroll_rect = element_rect(rt, "console-scroll");
    let visible = rt
        .elements()
        .into_iter()
        .filter(|el| {
            el.key
                .as_deref()
                .is_some_and(|k| k.starts_with("console/entry/") && k.ends_with("/sql"))
        })
        .find(|el| {
            let cy = el.rect[1] + el.rect[3] * 0.5;
            el.rect[2] > 20.0
                && cy >= scroll_rect[1] + 2.0
                && cy <= scroll_rect[1] + scroll_rect[3] - 2.0
        })
        .expect("visible console sql element");
    let idx = visible
        .key
        .as_deref()
        .expect("sql key")
        .strip_prefix("console/entry/")
        .and_then(|s| s.strip_suffix("/sql"))
        .and_then(|n| n.parse().ok())
        .expect("entry index");
    (idx, visible.rect)
}

fn drag_select_sql_line(rt: &mut Runtime<AppState>, rect: [f32; 4]) {
    let y = rect[1] + (rect[3] * 0.5).min(8.0);
    let x_start = rect[0] + 4.0;
    let x_end = (rect[0] + rect[2] - 4.0).min(x_start + 180.0);
    rt.pointer_down(x_start, y);
    rt.pointer_move(x_end, y);
    rt.pointer_up(x_end, y);
    rt.frame(0.0);
}

#[test]
fn console_log_pane_resizes_clamps_cancels_and_resets_on_double_click() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-connected-dark");
    rt.frame(0.0);

    let initial_rect = element_rect(&rt, "workspace-console");
    assert_eq!(initial_rect[3], CONSOLE_DEFAULT_HEIGHT);

    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_move(hx, hy - 100.0);
    rt.pointer_up(hx, hy - 100.0);
    rt.frame(0.0);
    assert_eq!(element_rect(&rt, "workspace-console")[3], 284.0);
    assert_eq!(
        rt.state.active_session().expect("session").console_height,
        284.0
    );

    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_move(hx, hy + 400.0);
    rt.pointer_up(hx, hy + 400.0);
    rt.frame(0.0);
    assert_eq!(element_rect(&rt, "workspace-console")[3], CONSOLE_MIN_HEIGHT);
    assert_eq!(
        rt.state.active_session().expect("session").console_height,
        CONSOLE_MIN_HEIGHT
    );

    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_move(hx, hy - 1000.0);
    rt.pointer_up(hx, hy - 1000.0);
    rt.frame(0.0);
    let main_h = element_rect(&rt, "workspace-main-inner")[3];
    let max_h = SessionState::console_max_height(main_h);
    let clamped_console_h = element_rect(&rt, "workspace-console")[3];
    assert_eq!(clamped_console_h, max_h);
    assert!(main_h - clamped_console_h >= CONSOLE_MIN_STAGE_HEIGHT);

    let saved_h = rt.state.active_session().expect("session").console_height;
    rt.set_viewport((1280.0, 560.0), 1.0);
    rt.frame(0.0);
    rt.frame(0.0);
    assert!(element_rect(&rt, "workspace-console")[3] < saved_h);
    assert_eq!(rt.state.active_session().expect("session").console_height, saved_h);

    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_move(hx, hy + 40.0);
    rt.frame(0.0);
    assert_ne!(
        rt.state.active_session().expect("session").console_height,
        saved_h
    );
    rt.key(KeyEvent::from(Key::Escape));
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").console_height, saved_h);

    rt.set_viewport((1280.0, 800.0), 1.0);
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(rt.state.active_session().expect("session").console_height, saved_h);
    assert_eq!(element_rect(&rt, "workspace-console")[3], saved_h);

    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_up(hx, hy);
    rt.frame(0.05);
    rt.pointer_down(hx, hy);
    rt.pointer_up(hx, hy);
    rt.frame(0.0);
    assert_eq!(
        rt.state.active_session().expect("session").console_height,
        CONSOLE_DEFAULT_HEIGHT
    );
    assert_eq!(
        element_rect(&rt, "workspace-console")[3],
        CONSOLE_DEFAULT_HEIGHT
    );
}

#[test]
fn console_log_max_resize_preserves_sql_workbench_results_viewport() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-sql-editor-dark");
    rt.frame(0.0);

    let tab_id = rt
        .state
        .active_session()
        .expect("session")
        .active_tab
        .expect("active tab")
        .get();
    let (hx, hy) = rt.find("console-resize-handle").expect("console resize handle");
    rt.pointer_down(hx, hy);
    rt.pointer_move(hx, hy - 1000.0);
    rt.pointer_up(hx, hy - 1000.0);
    rt.frame(0.0);
    rt.frame(0.0);

    let console_rect = element_rect(&rt, "workspace-console");
    let split_rect = element_rect(&rt, "sql-split-handle");
    let results_header_rect = element_rect(&rt, "query-results-header");
    let results_pane_rect = element_rect(&rt, &format!("query-results-pane-{tab_id}"));

    assert!(split_rect[1] + split_rect[3] <= console_rect[1]);
    assert!(results_header_rect[1] + results_header_rect[3] <= console_rect[1]);
    assert!(results_pane_rect[1] + results_pane_rect[3] <= console_rect[1]);
    assert!(results_pane_rect[3] - results_header_rect[3] >= 80.0);
}

#[test]
fn console_log_text_can_be_selected_and_copied_without_hijacking_inputs() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    let clipboard = rune::clipboard::memory();
    rt.use_clipboard(&clipboard);

    rt.update(|s, _| {
        if let Some(sess) = s.active_session_mut() {
            sess.console_entries.push(ConsoleLogEntry {
                timestamp: "2026-11-26 15:34:31.042".to_owned(),
                sql: "SELECT * FROM greendale_missing;".to_owned(),
                duration_ms: 2,
                error: Some("ERROR: relation \"greendale_missing\" does not exist".to_owned()),
            });
        }
    });
    rt.frame(0.0);

    let (visible_entry_idx, visible_sql_rect) = visible_console_sql(&rt);
    let expected_entry_sql = rt
        .state
        .active_session()
        .expect("session")
        .console_entries[visible_entry_idx]
        .sql
        .clone();

    drag_select_sql_line(&mut rt, visible_sql_rect);
    assert!(rt.selection().is_some());

    let cmd_mods = Mods {
        cmd: true,
        ..Default::default()
    };
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    let dragged_copy = clipboard.text().expect("copied selection");
    assert!(!dragged_copy.is_empty());
    assert!(
        expected_entry_sql.contains(dragged_copy.as_str()),
        "expected {:?} inside {expected_entry_sql:?}",
        dragged_copy.as_str()
    );

    rt.key(KeyEvent {
        key: Key::Command('a'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    let all_copied = clipboard.text().expect("copied all console entries");
    let entries = &rt.state.active_session().expect("session").console_entries;
    let first_meta = format!("-- {} ({} ms)", entries[0].timestamp, entries[0].duration_ms);
    let second_meta = format!("-- {} ({} ms)", entries[1].timestamp, entries[1].duration_ms);
    assert!(all_copied.contains(&first_meta), "missing first meta in {all_copied:?}");
    assert!(all_copied.contains(&entries[0].sql), "missing first sql in {all_copied:?}");
    assert!(
        all_copied.contains(&format!("\n\n{second_meta}")),
        "missing blank-line separator before second entry in {all_copied:?}"
    );
    assert!(
        all_copied.contains("ERROR: relation \"greendale_missing\" does not exist"),
        "missing error text in {all_copied:?}"
    );

    let (fx, fy) = rt.find("table-filter-input").expect("filter input");
    rt.pointer_down(fx, fy);
    rt.pointer_up(fx, fy);
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("gpa > 3.0".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent {
        key: Key::Command('a'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    assert_eq!(clipboard.text().as_deref().map(String::as_str), Some("gpa > 3.0"));

    let (after_idx, visible_sql_rect) = visible_console_sql(&rt);
    let after_expected_sql = rt
        .state
        .active_session()
        .expect("session")
        .console_entries[after_idx]
        .sql
        .clone();
    drag_select_sql_line(&mut rt, visible_sql_rect);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: cmd_mods,
    });
    rt.frame(0.0);
    let after_focus_copy = clipboard.text().expect("copied console selection after input focus");
    assert_ne!(after_focus_copy.as_str(), "gpa > 3.0");
    assert!(!after_focus_copy.is_empty());
    assert!(
        after_expected_sql.contains(after_focus_copy.as_str()),
        "expected {:?} inside {after_expected_sql:?}",
        after_focus_copy.as_str()
    );
}

#[test]
fn settings_history_toggle_persists_pauses_recording_and_omits_raw_errors() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let settings_path = dir.path().join("settings.json");

    let sqlite_profile = ConnectionProfile::new_sqlite("greendale_sqlite", ":memory:");
    let conn_id = sqlite_profile.id;
    let store = StoreData {
        connections: vec![sqlite_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    state.store_path = dir.path().join("connections.json");
    state.settings_path = settings_path.clone();
    state.persist_enabled = true;
    state.appearance = ColorScheme::Dark;

    let sqlite_connector: SessionConnector = Arc::new(|profile, password, db_override| {
        let profile = profile.clone();
        let password = password.clone();
        let db_override = db_override.clone();
        Box::pin(async move { connect_session(&profile, password.as_deref(), db_override.as_deref()).await })
    });
    state.worker = Some(WorkerBridge::spawn_with_connector(sqlite_connector).expect("spawn sqlite worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    // 1. Open Settings modal, verify HISTORY card, toggle recording OFF, and verify settings.json persistence.
    rt.update(|s, _| s.open_settings());
    rt.frame(0.1);
    assert!(rt.find("settings-history-card").is_some());
    assert!(has_text(&rt, "HISTORY"));
    assert!(has_text(&rt, "Record query history"));
    assert!(rt.state.record_query_history);

    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(!rt.state.record_query_history);

    let persisted_off = SettingsStore::at(settings_path.clone())
        .load()
        .expect("load settings with history disabled");
    assert!(!persisted_off.record_query_history);

    let (done_x, done_y) = rt.find("settings-done-btn").expect("settings done button");
    rt.pointer_down(done_x, done_y);
    rt.pointer_up(done_x, done_y);
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::None));

    // 2. Empty History sidebar explains when recording is paused, action bar shows History Off, and executed SQL is not recorded.
    rt.update(|s, tx| {
        if let Some(sess) = s.active_session_mut() {
            sess.sidebar_tab = SidebarTab::History;
        }
        s.open_new_query_tab(tx);
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "Query history recording is paused in Settings."));
    assert!(has_text(&rt, "History Off"));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT 1 AS troy_barnes;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| !t.running)
    });
    let session = rt.state.active_session().expect("session");
    assert!(session.query_history.is_empty(), "history must stay empty while paused");
    assert_eq!(
        session.active_query_tab().expect("query tab").results[0].statement.rows[0][0],
        CellValue::Int(1)
    );

    // Reload from worker QueryStore to prove nothing was persisted either (wait until sentinel clears).
    let session_id = session.id;
    rt.update(|s, _| {
        if let Some(sess) = s.active_session_mut() {
            sess.query_history.push(tv_core::QueryHistoryEntry {
                id: -999,
                connection_id: Some(conn_id),
                connection_name: "sentinel".to_owned(),
                database_name: None,
                sql: "SELECT 0".to_owned(),
                executed_at: chrono::Utc::now(),
                duration_ms: 0,
                rows_count: None,
                error: None,
            });
        }
        if let Some(worker) = &s.worker {
            worker.send(WorkerCmd::LoadQueryLibrary {
                session_id,
                connection_id: conn_id,
            });
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|s| s.query_history.iter().all(|h| h.id != -999))
    });
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .query_history
            .is_empty()
    );

    // 3. Re-enable global recording in Settings, then verify per-tab isolation & credential exclusion labels.
    rt.update(|s, _| s.open_settings());
    rt.frame(0.1);
    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(rt.state.record_query_history);
    assert!(
        SettingsStore::at(settings_path.clone())
            .load()
            .expect("reload enabled settings")
            .record_query_history
    );
    let (done_x, done_y) = rt.find("settings-done-btn").expect("settings done button");
    rt.pointer_down(done_x, done_y);
    rt.pointer_up(done_x, done_y);
    rt.frame(0.1);

    let tab1_id = rt
        .state
        .active_session()
        .and_then(|s| s.active_tab)
        .expect("tab 1 active");
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    let tab2_id = rt
        .state
        .active_session()
        .and_then(|s| s.active_tab)
        .expect("tab 2 active");
    assert_ne!(tab1_id, tab2_id);

    // Disable history in tab 1 only; global setting remains true.
    rt.update(|s, _| {
        s.active_session_mut().expect("session").active_tab = Some(tab1_id);
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History On"));
    let (ht_x, ht_y) = rt
        .find("query-history-toggle-btn")
        .expect("tab 1 history toggle button");
    rt.pointer_down(ht_x, ht_y);
    rt.pointer_up(ht_x, ht_y);
    rt.frame(0.05);
    assert!(has_text(&rt, "History Off"));
    assert!(rt.state.record_query_history);
    assert!(
        SettingsStore::at(settings_path)
            .load()
            .expect("global setting unchanged")
            .record_query_history
    );

    // Toggle tab 2 off then back on; tab 1 must stay off while tab 2 records.
    rt.update(|s, _| {
        s.active_session_mut().expect("session").active_tab = Some(tab2_id);
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History On"));
    let (ht2_x, ht2_y) = rt
        .find("query-history-toggle-btn")
        .expect("tab 2 history toggle button");
    rt.pointer_down(ht2_x, ht2_y);
    rt.pointer_up(ht2_x, ht2_y);
    rt.frame(0.05);
    assert!(has_text(&rt, "History Off"));
    rt.pointer_down(ht2_x, ht2_y);
    rt.pointer_up(ht2_x, ht2_y);
    rt.frame(0.05);
    assert!(has_text(&rt, "History On"));

    // Switch back to tab 1, verify it stays "History Off", and run SQL that must NOT be recorded.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        sess.active_tab = Some(tab1_id);
        let tab1 = sess.active_query_tab_mut().expect("tab 1");
        tab1.editor = rune::TextEdit::new("SELECT 10 AS troy;");
        assert!(s.run_active_query(true));
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History Off"));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| !t.running)
    });
    assert!(
        rt.state
            .active_session()
            .expect("session")
            .query_history
            .is_empty(),
        "tab 1 override off must not record queries even after tab 2 toggled on"
    );

    // Switch to tab 2 ("History On") and run a normal query -> recorded.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        sess.active_tab = Some(tab2_id);
        let tab2 = sess.active_query_tab_mut().expect("tab 2");
        tab2.editor = rune::TextEdit::new("SELECT 20 AS abed;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| !t.running)
    });
    assert_eq!(rt.state.active_session().expect("session").query_history.len(), 1);
    assert_eq!(
        rt.state.active_session().expect("session").query_history[0].sql,
        "SELECT 20 AS abed;"
    );

    // Credential-setting SQLite pragmas (plain, bracketed, quoted) display "History Skipped" and are excluded from history.
    for cred_sql in [
        "PRAGMA key = 'greendale-top-secret';",
        "PRAGMA [key] = 'greendale-bracket';",
        "PRAGMA main.'rekey' = 'greendale-quoted';",
    ] {
        rt.update(|s, _| {
            let tab = s
                .active_session_mut()
                .expect("session")
                .active_query_tab_mut()
                .expect("query tab");
            tab.editor = rune::TextEdit::new(cred_sql);
        });
        rt.frame(0.05);
        assert!(
            has_text(&rt, "History Skipped"),
            "expected History Skipped label for {cred_sql}"
        );
        assert!(
            !has_text(&rt, "History On"),
            "must not show History On for credential query {cred_sql}"
        );
        rt.update(|s, _| {
            assert!(s.run_active_query(true));
        });
        poll_until(&mut rt, Duration::from_secs(3), |rt| {
            rt.state
                .active_session()
                .and_then(|s| s.active_query_tab())
                .is_some_and(|t| !t.running)
        });
        assert_eq!(
            rt.state.active_session().expect("session").query_history.len(),
            1,
            "credential-setting SQL must not be recorded in query history: {cred_sql}"
        );
    }

    // Multi-statement buffer with a normal query at caret and credential SQL later shows "History: Current Only",
    // switching to "History Skipped" when moving the caret to the credential statement.
    let mixed_sql = "SELECT 1;\nPRAGMA [key] = 'greendale-placeholder';";
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(mixed_sql);
        tab.editor.anchor = 0;
        tab.editor.focus = 0;
    });
    rt.frame(0.05);
    assert!(
        has_text(&rt, "History: Current Only"),
        "expected History: Current Only when Run Current persists but Run All skips"
    );
    assert!(!has_text(&rt, "History On"));

    let pragma_pos = mixed_sql.find("PRAGMA").expect("find PRAGMA");
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor.anchor = pragma_pos;
        tab.editor.focus = pragma_pos;
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History Skipped"));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT * FROM greendale_secret_salaries;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| !t.running)
    });

    // User still sees the detailed backend error in the editor/console, while persisted history stores only "Query failed".
    let session = rt.state.active_session().expect("session");
    let raw_err = session
        .active_query_tab()
        .expect("query tab")
        .results[0]
        .statement
        .error
        .as_deref()
        .expect("live query error shown");
    assert!(
        raw_err.contains("greendale_secret_salaries"),
        "expected backend detail in live error, got {raw_err:?}"
    );
    assert_eq!(session.query_history.len(), 2);
    assert_eq!(session.query_history[0].error.as_deref(), Some("Query failed"));

    // Verify the persisted SQLite QueryStore rows contain only "SELECT 20 AS abed;" and the redacted failure.
    rt.update(|s, _| {
        if let Some(sess) = s.active_session_mut() {
            sess.query_history.clear();
        }
        if let Some(worker) = &s.worker {
            worker.send(WorkerCmd::LoadQueryLibrary {
                session_id,
                connection_id: conn_id,
            });
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|s| !s.query_history.is_empty())
    });
    let reloaded_history = &rt.state.active_session().expect("session").query_history;
    assert_eq!(reloaded_history.len(), 2);
    assert_eq!(reloaded_history[1].sql, "SELECT 20 AS abed;");
    let stored_err = reloaded_history[0].error.as_deref().expect("stored failure flag");
    assert_eq!(stored_err, "Query failed");
    assert!(!stored_err.contains("greendale_secret_salaries"));
    assert!(!stored_err.contains("no such table"));

    // 4. Failed settings save surfaces an error toast and banner inside Settings modal, then clears on retry.
    let blocked_settings_path = dir.path().join("blocked_settings_dir");
    std::fs::create_dir_all(&blocked_settings_path).expect("create blocking directory");
    rt.update(|s, _| {
        s.settings_path = blocked_settings_path;
        s.open_settings();
    });
    rt.frame(0.1);
    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(!rt.state.record_query_history);
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.is_error && n.message.contains("Failed to save settings")),
        "expected error toast on failed settings save"
    );
    assert!(rt.find("settings-save-error").is_some(), "expected settings modal save error");

    rt.update(|s, _| {
        s.settings_path = dir.path().join("recovered_settings.json");
    });
    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch on retry");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(rt.state.record_query_history);
    assert!(rt.state.settings_save_error.is_none());
    assert!(rt.state.status_notice.is_none(), "recovery save must clear settings failure toast");
    assert!(rt.find("settings-save-error").is_none());

    // Unrelated status notices remain visible across a settings recovery save.
    rt.update(|s, _| {
        s.settings_path = blocked_settings_path_recovery_check(dir.path());
    });
    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch for fail");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(rt.state.settings_save_error.is_some());

    rt.update(|s, _| {
        s.settings_path = dir.path().join("recovered_settings_2.json");
        s.set_notice("Unrelated Greendale notice", false);
    });
    let (sw_x, sw_y) = rt
        .find("settings-record-history-switch")
        .expect("record query history switch for unrelated notice");
    rt.pointer_down(sw_x, sw_y);
    rt.pointer_up(sw_x, sw_y);
    rt.frame(0.1);
    assert!(rt.state.settings_save_error.is_none());
    assert_eq!(
        rt.state.status_notice.as_ref().map(|n| n.message.as_str()),
        Some("Unrelated Greendale notice")
    );
}

fn blocked_settings_path_recovery_check(root: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("blocked_settings_dir_2");
    std::fs::create_dir_all(&p).expect("create second blocking directory");
    p
}

#[test]
fn mysql_no_backslash_escapes_run_current_uses_live_session_mode() {
    let mut mysql_profile = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy");
    mysql_profile.database = Some("greendale".to_owned());
    mysql_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = mysql_profile.id;

    let store = StoreData {
        connections: vec![mysql_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") =
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES)
    );

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let sql = "SELECT 'Troy\\'; SELECT 2; -- '";
    let caret = 18;
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(sql);
        tab.editor.anchor = caret;
        tab.editor.focus = caret;
    });
    rt.frame(0.05);

    let (btn_x, btn_y) = rt.find("query-run-cur-btn").expect("Run Current button");
    rt.pointer_down(btn_x, btn_y);
    rt.pointer_up(btn_x, btn_y);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
            && cluster
                .sessions
                .lock()
                .expect("sessions lock")
                .first()
                .is_some_and(|s| s.sql_calls.load(Ordering::Acquire) == 1)
    });

    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        let batches = sessions[0].executed_batches.lock().expect("executed_batches lock");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec!["SELECT 2".to_owned()]);
    }

    // SET SESSION sql_mode does not persist across pooled COM_RESET_CONNECTION resets,
    // so live session mysql_quote_mode stays at @@GLOBAL.sql_mode.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SET SESSION sql_mode = ''");
        tab.editor.anchor = 0;
        tab.editor.focus = 0;
    });
    rt.frame(0.05);
    let (btn_x, btn_y) = rt.find("query-run-cur-btn").expect("Run Current button");
    rt.pointer_down(btn_x, btn_y);
    rt.pointer_up(btn_x, btn_y);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
            && cluster
                .sessions
                .lock()
                .expect("sessions lock")
                .first()
                .is_some_and(|s| s.sql_calls.load(Ordering::Acquire) == 2)
    });
    assert_eq!(
        rt.state.active_session().and_then(|s| s.mysql_quote_mode()),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES)
    );

    // Switching @@GLOBAL.sql_mode updates the pooled connection mode.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SET GLOBAL sql_mode = ''");
        tab.editor.anchor = 0;
        tab.editor.focus = 0;
    });
    rt.frame(0.05);
    let (btn_x, btn_y) = rt.find("query-run-cur-btn").expect("Run Current button");
    rt.pointer_down(btn_x, btn_y);
    rt.pointer_up(btn_x, btn_y);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().and_then(|s| s.mysql_quote_mode())
            == Some(MysqlQuoteMode::DEFAULT)
    });

    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(sql);
        tab.editor.anchor = caret;
        tab.editor.focus = caret;
    });
    rt.frame(0.05);
    let (btn_x, btn_y) = rt.find("query-run-cur-btn").expect("Run Current button");
    rt.pointer_down(btn_x, btn_y);
    rt.pointer_up(btn_x, btn_y);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
            && cluster
                .sessions
                .lock()
                .expect("sessions lock")
                .first()
                .is_some_and(|s| s.sql_calls.load(Ordering::Acquire) == 4)
    });

    // Inline SET SESSION sql_mode followed by multiple statements via Run All splits all 3 statements.
    let inline_batch = "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'Troy\\'; SELECT 2; -- '";
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(inline_batch);
    });
    rt.frame(0.05);
    let (all_x, all_y) = rt.find("query-run-all-btn").expect("Run All button");
    rt.pointer_down(all_x, all_y);
    rt.pointer_up(all_x, all_y);
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
            && cluster
                .sessions
                .lock()
                .expect("sessions lock")
                .first()
                .is_some_and(|s| s.sql_calls.load(Ordering::Acquire) == 5)
    });

    let sessions = cluster.sessions.lock().expect("sessions lock");
    let batches = sessions[0].executed_batches.lock().expect("executed_batches lock");
    assert_eq!(batches[3], vec![sql.to_owned()]);
    assert_eq!(
        batches[4],
        vec![
            "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'".to_owned(),
            "SELECT 'Troy\\'".to_owned(),
            "SELECT 2".to_owned(),
        ]
    );
}

#[test]
fn mysql_stale_query_completion_after_db_switch_does_not_overwrite_quote_mode() {
    let mut mysql_profile = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy");
    mysql_profile.database = Some("greendale".to_owned());
    mysql_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = mysql_profile.id;

    let store = StoreData {
        connections: vec![mysql_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = Some(MysqlQuoteMode::DEFAULT);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        Some(MysqlQuoteMode::DEFAULT)
    );

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    let tab_id = rt
        .state
        .active_session()
        .and_then(|s| s.active_tab)
        .expect("query tab");

    // Hold DB A's execution so we can complete a DB switch while it is still in flight.
    cluster
        .sessions
        .lock()
        .expect("sessions lock")[0]
        .set_pre_dispatch_delay(Duration::from_millis(120));
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tab = sess
            .tab_mut(tab_id)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SELECT 1");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |_rt| {
        cluster
            .sessions
            .lock()
            .expect("sessions lock")
            .first()
            .is_some_and(|s| s.pre_dispatch_entries.load(Ordering::Acquire) == 1)
    });

    // Switch to DB B with NO_BACKSLASH_ESCAPES before DB A's slow query completes.
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") =
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES);
    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some_and(|c| c.active_database.as_deref() == Some("greendale_idp_dev"))
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES)
    );

    // Wait for DB A's in-flight query to finish; its stale QueryCompleted must not overwrite DB B's mode.
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == tab_id))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES)
    );
}

#[test]
fn mysql_batch_credentials_with_mid_batch_mode_change_excluded_from_history() {
    let mut mysql_profile = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy");
    mysql_profile.database = Some("greendale".to_owned());
    mysql_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = mysql_profile.id;

    let store = StoreData {
        connections: vec![mysql_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    state.record_query_history = true;
    let cluster = MockCluster::new(Duration::ZERO);
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") =
        Some(MysqlQuoteMode::ANSI_QUOTES);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // Statement 1 creates a user using ANSI_QUOTES + executable comment + # quote, while statement 2
    // resets GLOBAL sql_mode back to DEFAULT. Worker must evaluate history using the initial ANSI_QUOTES mode.
    let batch_sql = "CREATE USER \"Troy\\\" /*! IDENTIFIED BY 'greendale-placeholder' */ # \";\nSET GLOBAL sql_mode = '';";
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(batch_sql);
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
            && rt.state.active_session().and_then(|s| s.mysql_quote_mode())
                == Some(MysqlQuoteMode::DEFAULT)
    });

    let sess = rt.state.active_session().expect("session");
    assert!(
        sess.query_history.is_empty(),
        "credential batch followed by SET GLOBAL sql_mode must never be persisted to history"
    );
}

#[test]
fn corrupt_settings_load_pauses_history_and_surfaces_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let settings_path = dir.path().join("settings.json");

    // Missing settings.json starts cleanly with default history recording enabled.
    let mut rt = Runtime::new(
        AppState::init_in_memory(StoreData::default()),
        views::view,
        (1280.0, 800.0),
        Env::new(true, 1.0),
    );
    rt.update(|s, _| {
        s.persist_enabled = true;
        s.load_settings_from(settings_path.clone());
    });
    assert!(rt.state.record_query_history);
    assert!(rt.state.settings_save_error.is_none());
    assert!(rt.state.status_notice.is_none());

    // Corrupt existing settings.json must fail closed (history recording paused) and surface the load error.
    std::fs::write(
        &settings_path,
        r#"{"version":1,"appearance":"greendale_neon","record_query_history":false}"#,
    )
    .expect("write corrupt settings json");
    rt.update(|s, _| {
        s.load_settings_from(settings_path.clone());
        s.open_settings();
    });
    rt.frame(0.1);
    assert!(
        !rt.state.record_query_history,
        "corrupt settings.json must keep query history recording paused"
    );
    assert!(
        rt.state
            .settings_save_error
            .as_deref()
            .is_some_and(|e| e.contains("Failed to load settings")),
        "expected settings modal load error"
    );
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.is_error && n.message.starts_with("Failed to load settings:")),
        "expected error toast on failed settings load"
    );
    assert!(
        rt.find("settings-save-error").is_some(),
        "expected settings error banner in Settings modal"
    );

    // Saving valid settings clears both the banner and the load failure notice.
    rt.update(|s, _| {
        s.set_record_query_history(false);
    });
    rt.frame(0.1);
    assert!(rt.state.settings_save_error.is_none());
    assert!(
        rt.state.status_notice.is_none(),
        "valid settings save must clear failed settings load toast"
    );
    assert!(rt.find("settings-save-error").is_none());
    let reloaded = SettingsStore::at(settings_path.clone())
        .load()
        .expect("reloaded settings");
    assert!(!reloaded.record_query_history);

    // Dangling settings.json symlink must also pause recording and surface a load error.
    let dangling_path = dir.path().join("dangling_settings.json");
    std::os::unix::fs::symlink(dir.path().join("missing_target.json"), &dangling_path)
        .expect("create dangling symlink");
    rt.update(|s, _| {
        s.record_query_history = true;
        s.load_settings_from(dangling_path);
        s.open_settings();
    });
    rt.frame(0.1);
    assert!(!rt.state.record_query_history);
    assert!(
        rt.state
            .settings_save_error
            .as_deref()
            .is_some_and(|e| e.contains("Failed to load settings"))
    );
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.is_error && n.message.starts_with("Failed to load settings:"))
    );
    assert!(rt.find("settings-save-error").is_some());
}

#[test]
fn mysql_cancelled_global_mode_invalidation_and_standalone_run_current() {
    let mut mysql_profile = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy");
    mysql_profile.database = Some("greendale".to_owned());
    mysql_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = mysql_profile.id;

    let store = StoreData {
        connections: vec![mysql_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = Some(MysqlQuoteMode::DEFAULT);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        Some(MysqlQuoteMode::DEFAULT)
    );

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // Run Current inside `SELECT 'Troy\\'; SELECT 2; -- '` after an unexecuted SET SESSION
    // must resolve boundaries using the live DEFAULT mode rather than slicing at `';`.
    let sql = "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES';\nSELECT 'Troy\\'; SELECT 2; -- '";
    let caret = sql.find("SELECT 2").expect("SELECT 2 offset");
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(sql);
        tab.editor.anchor = caret;
        tab.editor.focus = caret;
        assert!(s.run_active_query(false));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        let batches = sessions[0].executed_batches.lock().expect("executed_batches lock");
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0],
            vec!["SELECT 'Troy\\'; SELECT 2; -- '".to_owned()],
            "Run Current must not dispatch an unterminated string using skipped SET SESSION mode"
        );
    }

    // Cancelled sql_mode batch reporting unknown mode (`None`) must invalidate stale cached mode.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("tab");
        let tab = sess.tab_mut(tid).and_then(|t| t.as_query_mut()).expect("query tab");
        tab.editor = rune::TextEdit::new("SET GLOBAL sql_mode = 'ANSI_QUOTES'; SELECT 1 /*UNKNOWN_MODE*/");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    assert_eq!(
        rt.state.active_session().expect("session").mysql_quote_mode(),
        None,
        "cancelled sql_mode batch with unknown mode must clear cached quote mode"
    );
}

#[test]
fn mysql_mode_change_before_dispatch_or_confirm_rejects_stale_approval() {
    let mut mysql_profile = ConnectionProfile::new_mysql("greendale_mysql", "127.0.0.1", "troy");
    mysql_profile.database = Some("greendale".to_owned());
    mysql_profile.safety = SafetyPolicy::ConfirmWrites;
    mysql_profile.env = EnvTag::Production;
    let conn_id = mysql_profile.id;

    let store = StoreData {
        connections: vec![mysql_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") = Some(MysqlQuoteMode::DEFAULT);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // 1. Worker reconnects into NO_BACKSLASH_ESCAPES after approval under DEFAULT -> must reject dispatch.
    {
        let sess0 = Arc::clone(&cluster.sessions.lock().expect("sessions lock")[0]);
        sess0.set_fail_next_pre_dispatch(true);
    }
    *cluster.mysql_quote_mode.lock().expect("mysql_quote_mode lock") =
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES);

    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SELECT 'Troy\\'; DROP TABLE students; -- '");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });

    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        assert_eq!(sessions.len(), 2, "expected reconnect to spawn replacement session");
        assert!(
            sessions[1]
                .executed_batches
                .lock()
                .expect("executed_batches lock")
                .is_empty(),
            "worker must not execute slices approved under old MySQL mode after reconnecting into new mode"
        );
    }
    let sess = rt.state.active_session().expect("session");
    assert_eq!(
        sess.mysql_quote_mode(),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES),
        "rejected dispatch must still refresh cached UI quote mode"
    );
    assert!(
        sess.active_query_tab()
            .and_then(|t| t.batch_error.as_deref())
            .is_some_and(|e| e.contains("changed before dispatch")),
        "expected mode mismatch error on tab"
    );

    // 2. Safety confirmation modal approved under NO_BACKSLASH_ESCAPES must reject if mode changes before Confirm.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("UPDATE students SET gpa = 4.0 WHERE student_id = 1;");
        assert!(s.run_active_query(true));
    });
    assert!(matches!(rt.state.overlay, tv_app::state::OverlayState::SafetyConfirm(_)));

    rt.update(|s, _| {
        if let Some(sess) = s.active_session_mut()
            && let ConnectionState::Connected(ref mut data) = sess.status
        {
            data.mysql_quote_mode = Some(MysqlQuoteMode::ANSI_QUOTES);
        }
        s.confirm_safety_modal();
    });
    assert!(matches!(rt.state.overlay, tv_app::state::OverlayState::None));
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.is_error && n.message.contains("MySQL session mode changed")),
        "expected error notice when session mode changes before confirmation"
    );
    let sessions = cluster.sessions.lock().expect("sessions lock");
    assert!(
        sessions[1]
            .executed_batches
            .lock()
            .expect("executed_batches lock")
            .is_empty(),
        "confirmation under stale mode must not dispatch SQL"
    );

    // 3. Pooled lease returning a different @@SESSION.sql_mode or server version at dispatch time
    // (while the session's cached mode still matched before checkout) must abort Query & EXPLAIN,
    // skip history, and sync UI quote mode to the live lease mode.
    assert_eq!(
        sessions[1].mysql_quote_mode(),
        Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES)
    );
    drop(sessions);

    let live_lease_mode_1 = MysqlQuoteMode::ANSI_QUOTES
        .with_server_version(tv_core::MysqlServerVersion::from_version_str("8.4.0"));
    let live_lease_mode_2 = MysqlQuoteMode::ANSI_QUOTES
        .with_server_version(tv_core::MysqlServerVersion::from_version_str("MariaDB 11.4.2"));

    rt.update(|s, _| {
        if let Some(sess) = s.active_session_mut()
            && let ConnectionState::Connected(ref mut data) = sess.status
        {
            data.mysql_quote_mode = Some(MysqlQuoteMode::NO_BACKSLASH_ESCAPES);
        }
    });
    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        sessions[1].set_next_lease_mysql_mode(Some(live_lease_mode_1));
    }

    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });

    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        assert!(sessions[1].executed_batches.lock().expect("executed_batches").is_empty());
        assert_eq!(sessions[1].sql_calls.load(Ordering::Acquire), 0);
        sessions[1].set_next_lease_mysql_mode(Some(live_lease_mode_2));
    }
    {
        let sess = rt.state.active_session().expect("session");
        assert_eq!(sess.mysql_quote_mode(), Some(live_lease_mode_1));
        assert!(sess.query_history.is_empty(), "mode mismatch must skip query history");
        assert!(
            sess.active_query_tab()
                .and_then(|t| t.batch_error.as_deref())
                .is_some_and(|e| e.contains("changed before dispatch")),
            "expected lease mode mismatch error on tab"
        );
    }

    // EXPLAIN with cached mode matching live_lease_mode_1 while the execution lease reports
    // MariaDB 11.4.2 must also reject pre-dispatch and update UI server version/mode.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query_explain(false));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.explain_loading)
    });

    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        assert_eq!(sessions[1].explain_calls.load(Ordering::Acquire), 0);
    }
    let sess = rt.state.active_session().expect("session");
    assert_eq!(sess.mysql_quote_mode(), Some(live_lease_mode_2));
    assert!(
        sess.active_query_tab()
            .and_then(|t| t.explain_error.as_deref())
            .is_some_and(|e| e.contains("changed before dispatch")),
        "expected EXPLAIN lease mode mismatch error on tab"
    );

    // 4. Switching between ANSI_QUOTES and MariaDB MSSQL on the lease must trigger MysqlModeMismatch.
    let live_lease_mode_3 = MysqlQuoteMode::MSSQL
        .with_server_version(tv_core::MysqlServerVersion::from_version_str("MariaDB 11.4.2"));
    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        sessions[1].set_next_lease_mysql_mode(Some(live_lease_mode_3));
    }
    rt.update(|s, _| {
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        assert_eq!(sess.mysql_quote_mode(), Some(live_lease_mode_3));
        assert!(
            sess.active_query_tab()
                .and_then(|t| t.batch_error.as_deref())
                .is_some_and(|e| e.contains("changed before dispatch"))
        );
    }

    // 5. Multi-statement slice inside an active executable comment is rejected on EXPLAIN and
    // Unrestricted Query dispatch before reaching the DB wire.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new("SELECT 1 /*! ; DROP TABLE students */");
        assert!(s.run_active_query_explain(false));
    });
    rt.frame(0.05);
    {
        let sessions = cluster.sessions.lock().expect("sessions lock");
        assert_eq!(sessions[1].explain_calls.load(Ordering::Acquire), 0);
        let sess = rt.state.active_session().expect("session");
        assert!(
            sess.active_query_tab()
                .and_then(|t| t.explain_error.as_deref())
                .is_some_and(|e| e.contains("single SQL statement"))
        );
    }

    // Also verify Worker/DbSession::execute_sql_cancellable_checked rejects multi-statement slices even under Unrestricted.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        sess.profile.safety = SafetyPolicy::Unrestricted;
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.batch_error = None;
        tab.editor = rune::TextEdit::new("SELECT 1 /*! ; SELECT 2 */");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let sessions = cluster.sessions.lock().expect("sessions lock");
    assert_eq!(sessions[1].sql_calls.load(Ordering::Acquire), 0);
    let sess = rt.state.active_session().expect("session");
    assert!(
        sess.active_query_tab()
            .and_then(|t| t.batch_error.as_deref())
            .is_some_and(|e| e.contains("multiple semicolon-separated statements"))
    );
}

#[test]
fn postgres_600_set_config_history_indicator_cached_and_records_history() {
    let mut pg_profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");
    pg_profile.database = Some("greendale".to_owned());
    pg_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = pg_profile.id;

    let store = StoreData {
        connections: vec![pg_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    let sql_600 = vec!["SELECT set_config('work_mem'::pg_catalog.text, '4MB', false)"; 600].join("; ");
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let tid = sess.active_tab.expect("query tab");
        let tab = sess
            .tab_mut(tid)
            .and_then(|t| t.as_query_mut())
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(&sql_600);
        tab.editor.anchor = 0;
        tab.editor.focus = 0;
    });

    let render_start = std::time::Instant::now();
    for _ in 0..3 {
        rt.frame(0.05);
        assert!(has_text(&rt, "History On"));
    }
    assert!(
        render_start.elapsed() < Duration::from_millis(500),
        "cached 600-statement History On frames took {:?}",
        render_start.elapsed()
    );

    rt.update(|s, _| {
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        let tab = sess.active_query_tab().expect("query tab");
        assert_eq!(tab.batch_error, None);
        assert_eq!(
            sess.query_history.len(),
            1,
            "600-statement non-secret set_config batch must record history"
        );
    }

    // Moving cursor or editing buffer invalidates the indicator cache cleanly.
    let mixed_sql = "SELECT 1;\nSELECT set_config('greendale.password', 'greendale-placeholder', false);";
    let secret_caret = mixed_sql.find("set_config").expect("set_config offset");
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab mut");
        tab.editor = rune::TextEdit::new(mixed_sql);
        tab.editor.anchor = 0;
        tab.editor.focus = 0;
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History: Current Only"));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab mut");
        tab.editor.anchor = secret_caret;
        tab.editor.focus = secret_caret;
    });
    rt.frame(0.05);
    assert!(has_text(&rt, "History Skipped"));
}

#[test]
fn postgres_standard_conforming_strings_tracks_live_mode_and_rejects_mismatch() {
    let mut pg_profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");
    pg_profile.database = Some("greendale".to_owned());
    pg_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = pg_profile.id;

    let store = StoreData {
        connections: vec![pg_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .postgres_standard_conforming_strings(),
        Some(true)
    );

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // 1. Toggle session standard_conforming_strings = off and verify UI sync.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SET standard_conforming_strings = off");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .postgres_standard_conforming_strings(),
        Some(false)
    );

    // 2. Run All and Run Current split using live scs = false.
    let escape_sql = "SELECT 'Troy\\'; Barnes'; SELECT 2;";
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(escape_sql);
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "SELECT 'Troy\\'; Barnes'".to_owned(),
                "SELECT 2".to_owned(),
            ])
        );
    }

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(escape_sql);
        tab.editor.anchor = 10;
        tab.editor.focus = 10;
        assert!(s.run_active_query(false));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec!["SELECT 'Troy\\'; Barnes'".to_owned()])
        );
    }

    // 3. Out-of-band lease scs change must abort before DB dispatch, sync UI mode, and skip history.
    let mock_sess = cluster.latest_session();
    let prev_sql_calls = mock_sess.sql_calls.load(Ordering::Acquire);
    let prev_hist_len = rt.state.active_session().expect("session").query_history.len();
    mock_sess.set_next_lease_pg_scs(Some(true));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.batch_error = None;
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });

    assert_eq!(mock_sess.sql_calls.load(Ordering::Acquire), prev_sql_calls);
    let sess = rt.state.active_session().expect("session");
    assert_eq!(sess.postgres_standard_conforming_strings(), Some(true));
    assert!(
        sess.active_query_tab()
            .and_then(|t| t.batch_error.as_deref())
            .is_some_and(|e| e.contains("string escaping mode changed before dispatch"))
    );
    assert_eq!(sess.query_history.len(), prev_hist_len);

    // 4. Live session scs change also blocks EXPLAIN pre-dispatch and syncs UI mode back to false.
    let prev_explain_calls = mock_sess.explain_calls.load(Ordering::Acquire);
    mock_sess.set_postgres_standard_conforming_strings(Some(false));
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT 42");
        tab.explain_error = None;
        assert!(s.run_active_query_explain(false));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.explain_loading)
    });
    assert_eq!(mock_sess.explain_calls.load(Ordering::Acquire), prev_explain_calls);
    let sess = rt.state.active_session().expect("session");
    assert_eq!(sess.postgres_standard_conforming_strings(), Some(false));
    assert!(
        sess.active_query_tab()
            .and_then(|t| t.explain_error.as_deref())
            .is_some_and(|e| e.contains("string escaping mode changed before dispatch"))
    );
}

#[test]
fn postgres_cross_run_transactions_conditional_set_config_and_reset_default() {
    let mut pg_profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");
    pg_profile.database = Some("greendale".to_owned());
    pg_profile.safety = SafetyPolicy::Unrestricted;
    let conn_id = pg_profile.id;

    let store = StoreData {
        connections: vec![pg_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // 1. Run 1: BEGIN sets in_transaction = Some(true).
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("BEGIN;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .postgres_session_mode()
            .and_then(|m| m.tx_state),
        Some(tv_core::PostgresTxState::InTransaction)
    );

    // Run 2: SET LOCAL off keeps escaped string intact across that batch.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SET LOCAL standard_conforming_strings = off; SELECT 'Troy\\'; Barnes';");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "SET LOCAL standard_conforming_strings = off".to_owned(),
                "SELECT 'Troy\\'; Barnes'".to_owned(),
            ])
        );
    }

    // Run 3: COMMIT restores scs = on and splits trailing statements cleanly.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("COMMIT; SELECT 'Abed\\' ; SELECT 2;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "COMMIT".to_owned(),
                "SELECT 'Abed\\'".to_owned(),
                "SELECT 2".to_owned(),
            ])
        );
    }
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .postgres_session_mode()
            .and_then(|m| m.tx_state),
        Some(tv_core::PostgresTxState::Idle)
    );

    // 2. Conditional CASE WHEN false THEN set_config(...) END does not merge subsequent SELECTs,
    // and direct checked execution rejects any unsplit multi-statement PG slice pre-dispatch.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(
            "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END; SELECT 'a\\' AS first; SELECT 'b' AS second -- ",
        );
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "SELECT CASE WHEN false THEN set_config('standard_conforming_strings', 'off', false) END".to_owned(),
                "SELECT 'a\\' AS first".to_owned(),
                "SELECT 'b' AS second --".to_owned(),
            ])
        );

        let async_rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let prev_calls = mock_sess.sql_calls.load(Ordering::Acquire);
        let rejected = async_rt.block_on(mock_sess.execute_sql_cancellable_checked(
            &["SELECT 'a\\' AS first; SELECT 'b' AS second -- ".to_owned()],
            None,
            1,
            None,
            SqlSessionMode::postgres(true),
        ));
        assert!(matches!(rejected, Err(DbError::InvalidInput(_))));
        assert_eq!(mock_sess.sql_calls.load(Ordering::Acquire), prev_calls);
    }

    // 3. Configured reset_standard_conforming_strings = Some(false) restores off on RESET.
    let reset_off_mode = PostgresSessionMode::new(false).with_reset_val(Some(false));
    cluster.latest_session().set_postgres_session_mode(Some(reset_off_mode));
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT 1");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    // UI received ModeMismatch on that stale dispatch and synced reset_off_mode; now run SET on; RESET; SELECT 'Troy\'; Barnes';
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(
            "SET standard_conforming_strings = on; RESET standard_conforming_strings; SELECT 'Troy\\'; Barnes';",
        );
        tab.batch_error = None;
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let mock_sess = cluster.latest_session();
    let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
    assert_eq!(
        batches.last().cloned(),
        Some(vec![
            "SET standard_conforming_strings = on".to_owned(),
            "RESET standard_conforming_strings".to_owned(),
            "SELECT 'Troy\\'; Barnes'".to_owned(),
        ])
    );
}

#[test]
fn postgres_aborted_tx_explain_analyze_and_cross_run_savepoints_stay_synced() {
    let mut pg_profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");
    pg_profile.database = Some("greendale".to_owned());
    pg_profile.env = EnvTag::Development;
    pg_profile.safety = SafetyPolicy::ConfirmWrites;
    let conn_id = pg_profile.id;

    let store = StoreData {
        connections: vec![pg_profile],
        ..StoreData::default()
    };
    let mut state = AppState::init_in_memory(store);
    let cluster = MockCluster::new(Duration::ZERO);
    state.worker = Some(WorkerBridge::spawn_with_connector(cluster.connector()).expect("spawn worker"));

    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);
    rt.frame(0.0);

    rt.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.status.as_connected())
            .is_some()
    });
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);

    // 1. Aborted tx rollback: BEGIN; SET scs=off; SELECT 1/0; leaves tx Aborted while keeping pre_tx = Some(true).
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("BEGIN; SET standard_conforming_strings = off; SELECT 1/0;");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let aborted_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(aborted_mode.tx_state, Some(tv_core::PostgresTxState::Aborted));
    assert!(!aborted_mode.standard_conforming_strings);
    assert_eq!(aborted_mode.pre_tx_standard_conforming_strings, Some(true));

    // Subsequent COMMIT rolls back to pre_tx = true so backslash is literal and statements split cleanly.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("COMMIT; SELECT 'Troy\\'; SELECT 2; -- '");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "COMMIT".to_owned(),
                "SELECT 'Troy\\'".to_owned(),
                "SELECT 2".to_owned(),
            ])
        );
    }
    let after_commit_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(after_commit_mode.tx_state, Some(tv_core::PostgresTxState::Idle));
    assert!(after_commit_mode.standard_conforming_strings);

    // 2. EXPLAIN ANALYZE changing session scs while transaction-local active scs is already off.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("BEGIN; SET LOCAL standard_conforming_strings = off;");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let before_explain_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert!(!before_explain_mode.standard_conforming_strings);
    assert_eq!(before_explain_mode.session_standard_conforming_strings, Some(true));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT set_config('standard_conforming_strings', 'off', false);");
        assert!(s.run_active_query_explain(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.explain_loading)
    });
    {
        let tab = rt
            .state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .expect("query tab");
        assert!(tab.explain_error.is_none());
    }
    let after_explain_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(
        after_explain_mode.tx_state,
        Some(tv_core::PostgresTxState::InTransaction)
    );
    assert_eq!(after_explain_mode.session_standard_conforming_strings, Some(false));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("COMMIT; SELECT 'Troy\\'; SELECT 2; -- '");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let mock_sess = cluster.latest_session();
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "COMMIT".to_owned(),
                "SELECT 'Troy\\'; SELECT 2; -- '".to_owned(),
            ])
        );
    }

    // 3. Cross-run named savepoint snapshot preserves active scs on ROLLBACK TO SAVEPOINT.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(
            "SET standard_conforming_strings = on; BEGIN; SET LOCAL standard_conforming_strings = off; SAVEPOINT greendale; SET LOCAL standard_conforming_strings = on;",
        );
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let sp_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert!(sp_mode.savepoints.iter().any(|sp| {
        sp.name == "greendale" && sp.active_standard_conforming_strings == Some(false)
    }));

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("ROLLBACK TO SAVEPOINT greendale; SELECT 'Troy\\'; SELECT 2; -- '");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let mock_sess = cluster.latest_session();
    let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
    assert_eq!(
        batches.last().cloned(),
        Some(vec![
            "ROLLBACK TO SAVEPOINT greendale".to_owned(),
            "SELECT 'Troy\\'; SELECT 2; -- '".to_owned(),
        ])
    );
    drop(batches);

    // 4. Failed COMMIT (e.g. deferred unique constraint) returns PostgreSQL to Idle so subsequent
    //    SET scs = off + SELECT 1 executes cleanly without being dropped under Aborted state.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("COMMIT; SET standard_conforming_strings = on; BEGIN; SET standard_conforming_strings = off;");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("COMMIT /*DEFERRED_COMMIT_FAIL*/;");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    let after_deferred_mode = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(after_deferred_mode.tx_state, Some(tv_core::PostgresTxState::Idle));
    assert!(after_deferred_mode.standard_conforming_strings);

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SET standard_conforming_strings = off; SELECT 1;");
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                "SET standard_conforming_strings = off".to_owned(),
                "SELECT 1".to_owned(),
            ])
        );
    }

    // 5. Cross-run savepoints sharing a 63-byte prefix restore the newer snapshot on ROLLBACK TO.
    let prefix = "g".repeat(60);
    let sp_first = format!("{prefix}abc_first");
    let sp_second = format!("{prefix}abc_second");
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(format!(
            "SET standard_conforming_strings = on; BEGIN; SAVEPOINT {sp_first}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {sp_second}; SET LOCAL standard_conforming_strings = on;"
        ));
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(format!(
            "ROLLBACK TO SAVEPOINT {sp_first}; SELECT 'Troy\\'; SELECT 2; -- '"
        ));
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                format!("ROLLBACK TO SAVEPOINT {sp_first}"),
                "SELECT 'Troy\\'; SELECT 2; -- '".to_owned(),
            ])
        );
    }

    // 6. History filter excludes computed set_config(array_to_string(ARRAY[...], ''), ...) and truncating varchar(N)[] casts.
    rt.update(|s, _| {
        let sess = s.active_session_mut().expect("session");
        let sql_mode = sess.sql_mode();
        let tab = sess.active_query_tab_mut().expect("query tab");
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config(array_to_string(ARRAY['greendale.pass', 'word'], ''), 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config(array_to_string(ARRAY['greendale.passwordextra']::varchar(18)[], ''), 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config(replace(array_to_string(ARRAY['greendale.db'::char(13), 'password'], ''), ' ', '_'), 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config(array_to_string(ARRAY['a'::varchar(18), 'greendale.passwordextra'], ','), 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "SELECT set_config(array_to_string(ARRAY['work_mem'::varchar(8)], ''), '4MB', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "SELECT set_config(array_to_string(ARRAY['work_' || 'mem'], ''), '4MB', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "SELECT set_config('work_mem'::pg_catalog.text, '4MB', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (false, false)
        );
        tab.editor = rune::TextEdit::new(
            "SELECT set_config('search_path', 'public', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (false, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT pg_catalog.set_config(N'greendale.app.password ', 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config('greendale.app.password_extra'::text, 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config((ARRAY['work_mem'] || '{greendale.password}')[2], 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode.clone()),
            (true, false)
        );
        tab.editor = rune::TextEdit::new(
            "ROLLBACK; SELECT set_config('greendale.pass' || (ARRAY['word'])[1], 'Troy Barnes', false);",
        );
        assert_eq!(
            tab.history_indicator_exclusions(DbEngine::Postgres, sql_mode),
            (true, false)
        );
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        assert!(
            !sess
                .query_history
                .iter()
                .any(|h| h.sql.contains("Troy Barnes") || h.sql.contains("greendale.pass")),
            "computed array_to_string credential statement must never enter query history"
        );
    }

    // 7. Non-UTF8 LATIN1 (SingleByte) encoding keeps 32x 'é' + 'a'/'b' savepoints distinct across runs.
    let lat_prefix = "é".repeat(32);
    let sp_lat_a = format!("\"{lat_prefix}a\"");
    let sp_lat_b = format!("\"{lat_prefix}b\"");
    mock_sess.set_postgres_session_mode(Some(
        PostgresSessionMode::new(true)
            .with_server_encoding(Some(tv_core::PostgresServerEncoding::from_pg_name("LATIN1"))),
    ));
    // Refresh session state through a harmless query so the UI adopts MockSession's LATIN1 encoding.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(format!(
            "BEGIN; SAVEPOINT {sp_lat_a}; SET LOCAL standard_conforming_strings = off; SAVEPOINT {sp_lat_b}; SET LOCAL standard_conforming_strings = on;"
        ));
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab");
        tab.editor = rune::TextEdit::new(format!(
            "ROLLBACK TO SAVEPOINT {sp_lat_a}; SELECT 'Troy\\'; SELECT 2; -- '"
        ));
        assert!(s.run_active_query(true));
        if matches!(s.overlay, OverlayState::SafetyConfirm(_)) {
            s.confirm_safety_modal();
        }
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.active_query_tab())
            .is_some_and(|t| t.running)
    });
    {
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec![
                format!("ROLLBACK TO SAVEPOINT {sp_lat_a}"),
                "SELECT 'Troy\\'".to_owned(),
                "SELECT 2".to_owned(),
            ])
        );
    }
    {
        let sess = rt.state.active_session().expect("session");
        assert!(
            !sess
                .query_history
                .iter()
                .any(|h| h.sql.contains("Troy Barnes") || h.sql.contains("greendale.db")),
            "padded char(n) replace array_to_string statement must never enter query history"
        );
    }
}

#[test]
fn postgres_aux_errors_staged_commit_and_concurrent_tabs_keep_mode_in_sync() {
    let cluster = MockCluster::new(Duration::ZERO);
    let mut rt = worker_runtime(&cluster);
    rt.state.connections[0].safety = SafetyPolicy::Unrestricted;
    rt.frame(0.0);

    connect_select_db_schema_and_students(&mut rt);
    let mock_sess = cluster.latest_session();
    let students_tab_id = rt
        .state
        .active_session()
        .expect("session")
        .active_tab
        .expect("students tab");

    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    let query_tab_a = rt
        .state
        .active_session()
        .expect("session")
        .active_tab
        .expect("query tab a");

    // 1. Grid filter query failure inside an active transaction aborts tx and updates UI session mode.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new("BEGIN; SET standard_conforming_strings = off;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == query_tab_a))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });
    let after_begin = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(after_begin.tx_state, Some(tv_core::PostgresTxState::InTransaction));
    assert!(!after_begin.standard_conforming_strings);

    rt.update(|s, tx| s.activate_tab(students_tab_id, tx));
    rt.frame(0.05);
    mock_sess.set_fail_next_page(true);
    let (fx, fy) = rt.find("table-filter-input").expect("filter input");
    rt.pointer_down(fx, fy);
    rt.pointer_up(fx, fy);
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Text("1/0 = 1".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| !t.page_loading && t.filter_error.is_some())
    });
    assert_eq!(
        rt.state
            .active_session()
            .expect("session")
            .postgres_session_mode()
            .and_then(|m| m.tx_state),
        Some(tv_core::PostgresTxState::Aborted)
    );

    rt.update(|s, tx| s.activate_tab(query_tab_a, tx));
    rt.frame(0.05);
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new("COMMIT; SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == query_tab_a))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        let tab = sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_a)
            .and_then(|t| t.as_query())
            .expect("query tab a");
        assert_eq!(tab.batch_error, None);
        let mode = sess.postgres_session_mode().expect("pg mode");
        assert_eq!(mode.tx_state, Some(tv_core::PostgresTxState::Idle));
        assert!(mode.standard_conforming_strings);
    }

    // 2. Staged table edit commit ends transaction and restores pre-tx SCS in UI session mode.
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new("BEGIN; SET LOCAL standard_conforming_strings = off;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == query_tab_a))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });

    rt.update(|s, tx| s.activate_tab(students_tab_id, tx));
    rt.frame(0.1);
    rt.update(|s, tx| {
        assert!(s.begin_cell_edit(RowKey::Existing(0), 1, tx));
    });
    rt.frame(0.1);
    rt.key(KeyEvent::from(Key::Text("Abed Nadir".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);
    rt.update(|s, _| s.commit_staged_changes());
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|s| s.active_table_tab())
            .is_some_and(|t| !t.committing && !t.staged.has_changes() && !t.page_loading)
    });
    let after_staged_commit = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert_eq!(after_staged_commit.tx_state, Some(tv_core::PostgresTxState::Idle));
    assert!(after_staged_commit.standard_conforming_strings);

    rt.update(|s, tx| s.activate_tab(query_tab_a, tx));
    rt.frame(0.05);
    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new("SELECT 'Troy\\';");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == query_tab_a))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        let tab = sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_a)
            .and_then(|t| t.as_query())
            .expect("query tab a");
        assert_eq!(tab.batch_error, None);
    }

    // 3. Delayed Tab A completion cannot revert Tab B's newer standard_conforming_strings = off change.
    rt.update(|s, tx| s.open_new_query_tab(tx));
    rt.frame(0.05);
    let query_tab_b = rt
        .state
        .active_session()
        .expect("session")
        .active_tab
        .expect("query tab b");

    let calls_before_a = mock_sess.sql_calls.load(Ordering::Acquire);
    mock_sess.set_sql_delay_once(Duration::from_millis(80));
    rt.update(|s, tx| {
        s.record_query_history = true;
        s.activate_tab(query_tab_a, tx);
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new("SELECT 1;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |_| {
        mock_sess.sql_calls.load(Ordering::Acquire) > calls_before_a
    });

    rt.update(|s, tx| {
        s.record_query_history = false;
        s.activate_tab(query_tab_b, tx);
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab b");
        tab.editor = rune::TextEdit::new("SET standard_conforming_strings = off;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        let Some(sess) = rt.state.active_session() else {
            return false;
        };
        let a_done = !sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_a)
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running);
        let b_done = !sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_b)
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running);
        a_done && b_done
    });
    let after_concurrent = rt
        .state
        .active_session()
        .expect("session")
        .postgres_session_mode()
        .expect("pg mode");
    assert!(!after_concurrent.standard_conforming_strings);

    rt.update(|s, _| {
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab b");
        tab.editor = rune::TextEdit::new("SELECT 'Troy\\'; SELECT 2; -- ");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        !rt.state
            .active_session()
            .and_then(|s| s.tabs.iter().find(|t| t.id == query_tab_b))
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running)
    });
    {
        let sess = rt.state.active_session().expect("session");
        let tab = sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_b)
            .and_then(|t| t.as_query())
            .expect("query tab b");
        assert_eq!(tab.batch_error, None);
        let batches = mock_sess.executed_batches.lock().expect("executed_batches lock");
        assert_eq!(
            batches.last().cloned(),
            Some(vec!["SELECT 'Troy\\'; SELECT 2; --".to_owned()])
        );
    }

    // 4. Cancelled query still classifies history under the dispatch-time SQL mode even if another tab changes mode first.
    let cred_under_off =
        "SELECT 'Troy\\'', set_config('greendale.password', 'greendale-placeholder', false) -- '";
    assert!(tv_core::batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &[cred_under_off.to_owned()],
        cred_under_off,
        SqlSessionMode::postgres(false),
    ));
    assert!(!tv_core::batch_sets_credentials_with_session_mode(
        DbEngine::Postgres,
        &[cred_under_off.to_owned()],
        cred_under_off,
        SqlSessionMode::postgres(true),
    ));

    let pre_entries = mock_sess.pre_dispatch_entries.load(Ordering::Acquire);
    mock_sess.set_pre_dispatch_delay(Duration::from_millis(120));
    rt.update(|s, tx| {
        s.record_query_history = true;
        s.activate_tab(query_tab_a, tx);
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab a");
        tab.editor = rune::TextEdit::new(cred_under_off);
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |_| {
        mock_sess.pre_dispatch_entries.load(Ordering::Acquire) > pre_entries
    });

    rt.update(|s, tx| {
        s.activate_tab(query_tab_a, tx);
        assert!(s.cancel_active_query());
    });
    mock_sess.set_pre_dispatch_delay(Duration::ZERO);
    rt.update(|s, tx| {
        s.activate_tab(query_tab_b, tx);
        let tab = s
            .active_session_mut()
            .expect("session")
            .active_query_tab_mut()
            .expect("query tab b");
        tab.editor = rune::TextEdit::new("SET standard_conforming_strings = on;");
        assert!(s.run_active_query(true));
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        let Some(sess) = rt.state.active_session() else {
            return false;
        };
        let a_done = !sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_a)
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running);
        let b_done = !sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_b)
            .and_then(|t| t.as_query())
            .is_some_and(|t| t.running);
        a_done && b_done
    });
    {
        let sess = rt.state.active_session().expect("session");
        let tab_a = sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_a)
            .and_then(|t| t.as_query())
            .expect("query tab a");
        let tab_b = sess
            .tabs
            .iter()
            .find(|t| t.id == query_tab_b)
            .and_then(|t| t.as_query())
            .expect("query tab b");
        assert!(tab_a.batch_cancelled);
        assert_eq!(tab_b.batch_error, None);
        let live_mode = sess.postgres_session_mode().expect("pg mode");
        assert!(live_mode.standard_conforming_strings);
        assert!(
            sess.query_history.iter().all(|h| {
                !h.sql.contains("greendale.password") && !h.sql.contains("greendale-placeholder")
            }),
            "cancelled query must still be filtered from history under dispatch-time scs=off"
        );
    }
}

#[test]
fn corrupt_connection_store_preserves_file_and_supports_recovery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("connections.json");

    let mut initial = StoreData::default();
    let greendale = ConnectionProfile::new_postgres("study_group_db", "127.0.0.1", "annie_edison");
    initial.upsert(greendale.clone());
    ConnectionStore::at(store_path.clone())
        .save(&initial)
        .expect("seed valid connections json");

    // Overwrite with corrupt JSON and load into launcher.
    let corrupt_payload = "{corrupt greendale connections json";
    std::fs::write(&store_path, corrupt_payload).expect("write corrupt connections json");

    let mut rt = empty_runtime(true);
    rt.update(|s, _| {
        s.persist_enabled = true;
        s.load_connections_from(store_path.clone());
    });
    rt.frame(0.1);

    assert!(rt.state.store_load_error.is_some());
    assert!(rt.state.connections.is_empty());
    assert!(rt.find("launcher-store-error").is_some());
    assert!(rt.find("launcher-store-retry").is_some());
    assert!(rt.find("launcher-store-reset").is_some());

    // Attempting to save a new connection must fail rather than clobbering the corrupt file.
    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("should_not_overwrite");
        }
        s.save_connection_form(false, tx);
    });
    rt.frame(0.1);
    assert!(matches!(rt.state.overlay, OverlayState::ConnectionForm(_)));
    if let OverlayState::ConnectionForm(ref form) = rt.state.overlay {
        assert!(
            form.validation_error
                .as_deref()
                .is_some_and(|e| e.contains("Failed to save connection profile")),
            "expected save block validation error, got {:?}",
            form.validation_error
        );
    }
    assert_eq!(
        std::fs::read_to_string(&store_path).expect("corrupt file unchanged"),
        corrupt_payload
    );
    assert!(rt.state.persist_connections().is_err());

    // Repair the file on disk and click Retry Load -> restores profiles and clears banner.
    rt.update(|s, _| s.dismiss_connection_form());
    ConnectionStore::at(store_path.clone())
        .save(&initial)
        .expect("restore repaired connections json");
    rt.frame(0.1);
    let (rx, ry) = rt.find("launcher-store-retry").expect("retry load button");
    rt.pointer_down(rx, ry);
    rt.pointer_up(rx, ry);
    rt.frame(0.1);

    assert!(rt.state.store_load_error.is_none());
    assert!(rt.find("launcher-store-error").is_none());
    assert_eq!(rt.state.connections.len(), 1);
    assert_eq!(rt.state.connections[0].name, "study_group_db");

    // Corrupt again and click Back Up & Start Fresh -> preserves backup file and re-enables saves.
    let second_corrupt = "{second corrupt greendale store";
    std::fs::write(&store_path, second_corrupt).expect("write second corrupt json");
    rt.update(|s, _| s.load_connections_from(store_path.clone()));
    rt.frame(0.1);
    assert!(rt.find("launcher-store-reset").is_some());

    let (bx, by) = rt.find("launcher-store-reset").expect("backup & reset button");
    rt.pointer_down(bx, by);
    rt.pointer_up(bx, by);
    rt.frame(0.1);

    assert!(rt.state.store_load_error.is_none());
    assert!(rt.find("launcher-store-error").is_none());
    assert!(rt.state.connections.is_empty());

    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".backup-"))
        .collect();
    assert_eq!(backups.len(), 1, "expected exactly one backup file");
    assert_eq!(
        std::fs::read_to_string(backups[0].path()).expect("read backup payload"),
        second_corrupt
    );

    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("dean_office_db");
        }
        s.save_connection_form(false, tx);
    });
    rt.frame(0.1);
    let persisted = ConnectionStore::at(store_path).load().expect("reload fresh store");
    assert_eq!(persisted.connections.len(), 1);
    assert_eq!(persisted.connections[0].name, "dean_office_db");
}

struct MockCredentialBackend {
    secrets: Mutex<std::collections::HashMap<String, (String, String)>>,
    ops_log: Mutex<Vec<String>>,
    fail_read: Mutex<Option<String>>,
    fail_write: Mutex<Option<String>>,
    fail_delete: Mutex<Option<String>>,
    delete_chmod_readonly: Mutex<Option<std::path::PathBuf>>,
    write_delay: Mutex<Duration>,
    write_calls: AtomicUsize,
}

impl MockCredentialBackend {
    fn new() -> Self {
        Self {
            secrets: Mutex::new(std::collections::HashMap::new()),
            ops_log: Mutex::new(Vec::new()),
            fail_read: Mutex::new(None),
            fail_write: Mutex::new(None),
            fail_delete: Mutex::new(None),
            delete_chmod_readonly: Mutex::new(None),
            write_delay: Mutex::new(Duration::ZERO),
            write_calls: AtomicUsize::new(0),
        }
    }
}

impl CredentialBackend for MockCredentialBackend {
    fn read(&self, service: &str) -> Result<Option<String>, String> {
        self.ops_log
            .lock()
            .expect("ops_log")
            .push(format!("read:{service}"));
        if let Some(err) = self.fail_read.lock().expect("fail_read").clone() {
            return Err(err);
        }
        Ok(self
            .secrets
            .lock()
            .expect("secrets")
            .get(service)
            .map(|(_, secret)| secret.clone()))
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        self.write_calls.fetch_add(1, Ordering::AcqRel);
        self.ops_log
            .lock()
            .expect("ops_log")
            .push(format!("start_write:{service}:{secret}"));
        let delay = *self.write_delay.lock().expect("write_delay");
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        if let Some(err) = self.fail_write.lock().expect("fail_write").clone() {
            self.ops_log
                .lock()
                .expect("ops_log")
                .push(format!("write:{service}:{secret}"));
            return Err(err);
        }
        self.secrets
            .lock()
            .expect("secrets")
            .insert(service.to_owned(), (account.to_owned(), secret.to_owned()));
        self.ops_log
            .lock()
            .expect("ops_log")
            .push(format!("write:{service}:{secret}"));
        Ok(())
    }

    fn delete(&self, service: &str) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;

        self.ops_log
            .lock()
            .expect("ops_log")
            .push(format!("delete:{service}"));
        if let Some(path) = self.delete_chmod_readonly.lock().expect("delete_chmod_readonly").as_ref() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
                .expect("chmod store readonly during delete");
        }
        if let Some(err) = self.fail_delete.lock().expect("fail_delete").clone() {
            return Err(err);
        }
        self.secrets.lock().expect("secrets").remove(service);
        Ok(())
    }
}

#[test]
fn profile_and_keychain_save_failures_report_recover_and_serialize() {
    use std::os::unix::fs::PermissionsExt;

    struct RestoreWritable(std::path::PathBuf);
    impl Drop for RestoreWritable {
        fn drop(&mut self) {
            drop(std::fs::set_permissions(
                &self.0,
                std::fs::Permissions::from_mode(0o644),
            ));
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("connections.json");
    let store = ConnectionStore::at(store_path.clone());
    store.save(&StoreData::default()).expect("seed empty store");

    let cluster = MockCluster::new(Duration::ZERO);
    cluster.set_expected_password(Some("human_beings"));
    let cred_backend = Arc::new(MockCredentialBackend::new());
    let cred_trait: Arc<dyn CredentialBackend> = cred_backend.clone();

    let mut state = AppState::init_in_memory(StoreData::default());
    state.persist_enabled = true;
    state.store_path = store_path.clone();
    state.appearance = ColorScheme::Dark;
    state.worker = Some(
        WorkerBridge::spawn_with_connector_and_credentials(cluster.connector(), cred_trait)
            .expect("spawn worker"),
    );
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);

    // 1. Keychain write failure on Save & Connect rolls back JSON, keeps form open, and does NOT connect.
    *cred_backend.fail_write.lock().expect("fail_write") =
        Some("keychain access denied".to_owned());
    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("greendale_pg");
            form.user = rune::TextEdit::new("troy_barnes");
            form.password = rune::TextEdit::new("human_beings");
            form.password_touched = true;
        }
        s.save_connection_form(true, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    assert_eq!(rt.state.screen, Screen::Launcher);
    assert_eq!(cluster.connect_count(), 0);
    assert!(rt.state.connections.is_empty());
    assert!(store.load().expect("reload after rollback").connections.is_empty());
    assert!(matches!(rt.state.overlay, OverlayState::ConnectionForm(_)));
    if let OverlayState::ConnectionForm(ref form) = rt.state.overlay {
        assert!(form.saving_req_id.is_none());
        assert!(
            form.validation_error
                .as_deref()
                .is_some_and(|e| e.contains("keychain access denied") && e.contains("reverted")),
            "unexpected validation error: {:?}",
            form.validation_error
        );
    }

    // 2. Retry Save & Connect after allowing keychain writes persists both stores and connects via keychain.
    *cred_backend.fail_write.lock().expect("fail_write") = None;
    rt.update(|s, tx| s.save_connection_form(true, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
            && rt
                .state
                .active_session()
                .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
    });
    let saved_id = rt
        .state
        .connections
        .first()
        .expect("saved profile")
        .id;
    let service = rt.state.connections[0].credential_service();
    assert_eq!(store.load().expect("reload saved").connections.len(), 1);
    assert_eq!(
        cred_backend
            .secrets
            .lock()
            .expect("secrets")
            .get(&service)
            .map(|(a, s)| (a.as_str(), s.as_str())),
        Some(("troy_barnes", "human_beings"))
    );
    let ops = cred_backend.ops_log.lock().expect("ops_log").clone();
    let write_pos = ops
        .iter()
        .position(|op| op == &format!("write:{service}:human_beings"))
        .expect("expected keychain write");
    let read_pos = ops
        .iter()
        .rposition(|op| op == &format!("read:{service}"))
        .expect("expected Save & Connect to read from keychain");
    assert!(write_pos < read_pos, "keychain write must complete before connect read: {ops:?}");

    // 2b. Save & Connect on a failed Keychain session does NOT cache the form password when keychain read fails.
    *cred_backend.fail_read.lock().expect("fail_read") =
        Some("initial read blocked".to_owned());
    rt.update(|s, tx| s.reconnect_active(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Failed(_)))
    });
    let connects_before_s1 = cluster.connect_count();
    *cred_backend.fail_read.lock().expect("fail_read") =
        Some("keychain read denied".to_owned());
    rt.update(|s, tx| {
        s.open_edit_connection_form(saved_id, tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.password = rune::TextEdit::new("human_beings");
            form.password_touched = true;
        }
        s.save_connection_form(true, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
            && rt.state.active_session().is_some_and(|sess| {
                matches!(&sess.status, ConnectionState::Failed(msg) if msg.contains("keychain read denied"))
            })
    });
    assert_eq!(cluster.connect_count(), connects_before_s1);
    let sess = rt.state.active_session().expect("active session");
    assert!(sess.session_password.is_none());
    *cred_backend.fail_read.lock().expect("fail_read") = None;
    rt.update(|s, tx| s.reconnect_active(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
    });
    assert_eq!(cluster.connect_count(), connects_before_s1 + 1);

    // 2c. Save & Connect while already connected forces a fresh connection using the edited database and Keychain read.
    cred_backend.ops_log.lock().expect("ops_log").clear();
    let connects_before_db_edit = cluster.connect_count();
    rt.update(|s, tx| {
        s.open_edit_connection_form(saved_id, tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.database = rune::TextEdit::new("greendale_new");
        }
        s.save_connection_form(true, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
            && rt.state.active_session().is_some_and(|sess| {
                sess.status
                    .as_connected()
                    .is_some_and(|c| c.active_database.as_deref() == Some("greendale_new"))
            })
    });
    assert_eq!(cluster.connect_count(), connects_before_db_edit + 1);
    assert!(
        cred_backend
            .ops_log
            .lock()
            .expect("ops_log")
            .contains(&format!("read:{service}")),
        "expected Save & Connect reconnect to read password from keychain"
    );

    // 3. Unwritable connections.json aborts Save BEFORE touching keychain.
    std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o444))
        .expect("chmod store 0444");
    let _restore = RestoreWritable(store_path.clone());
    let writes_before = cred_backend.write_calls.load(Ordering::Acquire);
    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("abed_pg");
            form.password = rune::TextEdit::new("cool_cool_cool");
            form.password_touched = true;
        }
        s.save_connection_form(false, tx);
    });
    rt.frame(0.1);
    assert!(rt.state.pending_profile_save.is_none());
    assert_eq!(cred_backend.write_calls.load(Ordering::Acquire), writes_before);
    assert!(matches!(rt.state.overlay, OverlayState::ConnectionForm(_)));
    if let OverlayState::ConnectionForm(ref form) = rt.state.overlay {
        assert!(
            form.validation_error
                .as_deref()
                .is_some_and(|e| e.contains("Failed to save connection profile")),
            "expected JSON save failure error, got {:?}",
            form.validation_error
        );
    }
    assert_eq!(store.load().expect("store unchanged").connections.len(), 1);

    // 3b. Failing to persist last_connected_at after reconnect surfaces an error notice while staying connected.
    rt.update(|s, tx| {
        s.dismiss_connection_form();
        s.status_notice = None;
        s.reconnect_active(tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
            && rt.state.status_notice.is_some()
    });
    assert!(
        rt.state.status_notice.as_ref().is_some_and(|n| {
            n.is_error && n.message.contains("failed to update connection metadata on disk")
        }),
        "expected metadata persistence error notice, got {:?}",
        rt.state.status_notice
    );

    // 3c. Switching away from Keychain when delete + JSON rollback both fail retains cleanup until Retry Save succeeds.
    drop(_restore);
    let _restore_s2 = RestoreWritable(store_path.clone());
    *cred_backend.fail_delete.lock().expect("fail_delete") =
        Some("keychain locked".to_owned());
    *cred_backend
        .delete_chmod_readonly
        .lock()
        .expect("delete_chmod_readonly") = Some(store_path.clone());
    rt.update(|s, tx| {
        s.open_edit_connection_form(saved_id, tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.password_mode = tv_core::PasswordMode::AskEveryTime;
        }
        s.save_connection_form(false, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    assert_eq!(
        rt.state.connections[0].password_mode,
        tv_core::PasswordMode::AskEveryTime
    );
    assert!(rt.state.pending_credential_deletes.contains(&saved_id));
    assert!(
        cred_backend
            .secrets
            .lock()
            .expect("secrets")
            .contains_key(&service)
    );
    if let OverlayState::ConnectionForm(ref form) = rt.state.overlay {
        assert!(
            form.validation_error
                .as_deref()
                .is_some_and(|e| e.contains("failed to revert") && e.contains("Retry Save")),
            "expected rollback failure guidance, got {:?}",
            form.validation_error
        );
    } else {
        panic!("expected ConnectionForm to remain open after rollback failure");
    }

    drop(_restore_s2);
    *cred_backend.fail_delete.lock().expect("fail_delete") = None;
    *cred_backend
        .delete_chmod_readonly
        .lock()
        .expect("delete_chmod_readonly") = None;
    rt.update(|s, tx| s.save_connection_form(false, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
            && !rt.state.pending_credential_deletes.contains(&saved_id)
    });
    assert!(
        !cred_backend
            .secrets
            .lock()
            .expect("secrets")
            .contains_key(&service),
        "Retry Save should remove the lingering keychain credential"
    );
    assert!(matches!(rt.state.overlay, OverlayState::None));

    // Re-save Keychain password before Step 4 tests profile deletion cleanup.
    rt.update(|s, tx| {
        s.open_edit_connection_form(saved_id, tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.password_mode = tv_core::PasswordMode::Keychain;
            form.password = rune::TextEdit::new("human_beings");
            form.password_touched = true;
        }
        s.save_connection_form(false, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none() && matches!(rt.state.overlay, OverlayState::None)
    });

    // 4. Keychain delete failure rolls back connections.json and keeps DeleteConfirm open with error.
    *cred_backend.fail_delete.lock().expect("fail_delete") =
        Some("keychain delete blocked".to_owned());
    rt.update(|s, _| {
        s.open_delete_confirm(saved_id);
        s.delete_connection(saved_id);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_delete.is_none()
    });
    assert_eq!(rt.state.connections.len(), 1);
    assert_eq!(store.load().expect("rolled back delete").connections.len(), 1);
    assert!(
        rt.state
            .delete_confirm_error
            .as_deref()
            .is_some_and(|e| e.contains("keychain delete blocked")),
        "expected delete error, got {:?}",
        rt.state.delete_confirm_error
    );
    assert!(rt.find("delete-confirm-error").is_some());

    *cred_backend.fail_delete.lock().expect("fail_delete") = None;
    rt.update(|s, _| s.delete_connection(saved_id));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_delete.is_none() && rt.state.connections.is_empty()
    });
    assert!(store.load().expect("deleted from disk").connections.is_empty());
    assert!(
        !cred_backend
            .secrets
            .lock()
            .expect("secrets")
            .contains_key(&service)
    );
    assert_eq!(rt.state.screen, Screen::Launcher);

    // 5. Concurrent credential updates for the same profile run strictly in FIFO order.
    let fifo_service = "tableview.connection.greendale-fifo".to_owned();
    cred_backend.ops_log.lock().expect("ops_log").clear();
    *cred_backend.write_delay.lock().expect("write_delay") = Duration::from_millis(80);
    let worker = rt.state.worker.as_ref().expect("worker");
    let r1 = tv_core::RequestId::new(9001);
    let r2 = tv_core::RequestId::new(9002);
    worker.send(WorkerCmd::SaveCredential {
        req_id: r1,
        connection_id: saved_id,
        service: fifo_service.clone(),
        account: "jeff_winger".to_owned(),
        secret: "first-secret".to_owned(),
    });
    worker.send(WorkerCmd::SaveCredential {
        req_id: r2,
        connection_id: saved_id,
        service: fifo_service.clone(),
        account: "jeff_winger".to_owned(),
        secret: "second-secret".to_owned(),
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if cred_backend.ops_log.lock().expect("ops_log").len() >= 4 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for serialized credential ops");
        std::thread::sleep(Duration::from_millis(10));
    }
    *cred_backend.write_delay.lock().expect("write_delay") = Duration::ZERO;
    assert_eq!(
        *cred_backend.ops_log.lock().expect("ops_log"),
        vec![
            format!("start_write:{fifo_service}:first-secret"),
            format!("write:{fifo_service}:first-secret"),
            format!("start_write:{fifo_service}:second-secret"),
            format!("write:{fifo_service}:second-secret"),
        ]
    );
    assert_eq!(
        cred_backend
            .secrets
            .lock()
            .expect("secrets")
            .get(&fifo_service)
            .map(|(_, s)| s.as_str()),
        Some("second-secret")
    );
}

#[test]
fn pending_save_preserves_concurrent_recent_timestamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("connections.json");
    let store = ConnectionStore::at(store_path.clone());
    store.save(&StoreData::default()).expect("seed empty store");

    let cluster = MockCluster::new(Duration::ZERO);
    let cred_backend = Arc::new(MockCredentialBackend::new());
    let cred_trait: Arc<dyn CredentialBackend> = cred_backend.clone();

    let mut state = AppState::init_in_memory(StoreData::default());
    state.persist_enabled = true;
    state.store_path = store_path;
    state.appearance = ColorScheme::Dark;
    state.worker = Some(
        WorkerBridge::spawn_with_connector_and_credentials(cluster.connector(), cred_trait)
            .expect("spawn worker"),
    );
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);

    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("greendale_pg");
            form.user = rune::TextEdit::new("troy_barnes");
            form.password = rune::TextEdit::new("human_beings");
            form.password_touched = true;
        }
        s.save_connection_form(true, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
            && rt
                .state
                .active_session()
                .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
    });
    let greendale_id = rt.state.connections[0].id;

    rt.update(|s, _| {
        s.connections[0].last_connected_at = None;
    });
    let mut reset_disk = store.load().expect("reload seeded store");
    reset_disk.connections[0].last_connected_at = None;
    store.save(&reset_disk).expect("save reset timestamp");

    *cred_backend.write_delay.lock().expect("write_delay") = Duration::from_millis(300);
    rt.update(|s, tx| {
        s.open_new_connection_form(tx);
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("abed_pg");
            form.user = rune::TextEdit::new("abed_nadir");
            form.password = rune::TextEdit::new("cool_cool_cool");
            form.password_touched = true;
        }
        s.save_connection_form(false, tx);
    });
    assert!(rt.state.pending_profile_save.is_some());

    rt.update(|s, tx| s.reconnect_active(tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .connections
            .iter()
            .find(|c| c.id == greendale_id)
            .and_then(|c| c.last_connected_at)
            .is_some()
    });
    assert!(
        rt.state.pending_profile_save.is_some(),
        "reconnect should finish while profile save is still pending"
    );

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none() && rt.state.connections.len() == 2
    });
    *cred_backend.write_delay.lock().expect("write_delay") = Duration::ZERO;

    assert!(
        rt.state
            .connections
            .iter()
            .find(|c| c.id == greendale_id)
            .and_then(|c| c.last_connected_at)
            .is_some(),
        "in-memory Recent timestamp should be preserved across pending save completion"
    );
    let reloaded = store.load().expect("reload after merged save");
    assert_eq!(reloaded.connections.len(), 2);
    assert!(
        reloaded
            .connections
            .iter()
            .find(|c| c.id == greendale_id)
            .and_then(|c| c.last_connected_at)
            .is_some(),
        "disk Recent timestamp should be persisted after pending save completion"
    );
}

#[test]
fn staged_edits_preserved_across_reload_close_db_switch_fk_jump_and_races() {
    let mut rt = sample_runtime(true);
    apply_scene(&mut rt, "workspace-data-grid-dark");
    rt.frame(0.5);

    rt.update(|s, tx| {
        s.begin_cell_edit(RowKey::Existing(0), 2, tx);
    });
    rt.frame(0.2);
    rt.key(KeyEvent::from(Key::Text("Troy Updated".to_owned())));
    rt.frame(0.0);
    rt.key(KeyEvent::from(Key::Enter));
    rt.frame(0.1);

    let (table_tab_id, schema, obj) = {
        let sess = rt.state.active_session().expect("session");
        let tab = sess.active_table_tab().expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        (tab.tab_id, tab.schema.clone(), tab.object_name.clone())
    };

    // 1. Reload is blocked when edits are staged.
    rt.update(|s, _| s.reload_active_tab());
    rt.frame(0.1);
    {
        let tab = rt
            .state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        assert!(tab.diff_drawer_open);
        assert!(
            rt.state
                .status_notice
                .as_ref()
                .is_some_and(|n| n.message.contains("reloading table"))
        );
    }

    // 2. Close tab is blocked and focuses/keeps the dirty tab.
    rt.update(|s, tx| {
        s.close_staged_diff_drawer();
        s.close_tab(table_tab_id, tx);
    });
    rt.frame(0.1);
    {
        let sess = rt.state.active_session().expect("session");
        assert!(sess.tabs.iter().any(|t| t.id == table_tab_id));
        let tab = sess.active_table_tab().expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        assert!(tab.diff_drawer_open);
        assert!(
            rt.state
                .status_notice
                .as_ref()
                .is_some_and(|n| n.message.contains("closing tab"))
        );
    }

    // 3. Database switch is blocked when any tab in the session has staged edits.
    rt.update(|s, tx| {
        s.close_staged_diff_drawer();
        s.select_database("greendale_other".to_owned(), tx);
    });
    rt.frame(0.1);
    {
        let sess = rt.state.active_session().expect("session");
        let tab = sess.active_table_tab().expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        assert!(tab.diff_drawer_open);
        assert!(
            rt.state
                .status_notice
                .as_ref()
                .is_some_and(|n| n.message.contains("switching database"))
        );
    }

    // 4. FK jump onto an already-open dirty tab focuses it without clearing edits or replacing its filter.
    let source_col = ColumnMeta {
        name: "student_id".to_owned(),
        data_type: "integer".to_owned(),
        udt_name: "int4".to_owned(),
        nullable: false,
        default_expr: None,
        is_primary_key: true,
        is_unique: true,
        comment: None,
        foreign_key: None,
    };
    rt.update(|s, tx| {
        s.close_staged_diff_drawer();
        s.open_new_query_tab(tx);
        s.jump_to_foreign_key(
            tv_core::ForeignKeyRef {
                constraint_name: "fk_student".to_owned(),
                target_schema: schema.clone(),
                target_table: obj.clone(),
                target_column: "student_id".to_owned(),
            },
            CellValue::Int(1),
            source_col,
            tx,
        );
    });
    rt.frame(0.1);
    {
        let sess = rt.state.active_session().expect("session");
        assert_eq!(sess.active_tab, Some(table_tab_id));
        let tab = sess.active_table_tab().expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        assert!(tab.applied_filter.is_none());
        assert!(tab.diff_drawer_open);
    }

    // 5. Mutations and discard are frozen while a commit is in flight.
    rt.update(|s, tx| {
        if let Some(tab) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
            tab.committing = true;
            tab.commit_req_id = Some(tv_core::RequestId::new(999));
            tab.commit_staged_gen = Some(tab.staged_gen);
        }
        assert!(!s.insert_staged_row(tx));
        s.toggle_delete_row(RowKey::Existing(1));
        s.stage_cell_null(RowKey::Existing(0), 3);
        s.discard_staged_changes();
    });
    {
        let tab = rt
            .state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab");
        assert_eq!(tab.staged.update_count(), 1);
        assert_eq!(tab.staged.insert_count(), 0);
        assert_eq!(tab.staged.delete_count(), 0);
    }

    // 6. Late TablePageLoaded arriving after edits are staged does not discard them.
    let cluster = MockCluster::new(Duration::ZERO);
    let mut rt_worker = worker_runtime(&cluster);
    let conn_id = rt_worker.state.connections[0].id;
    rt_worker.update(|s, tx| s.start_or_focus_connection(conn_id, None, None, tx));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
    });
    rt_worker.update(|s, tx| s.select_object("public".to_owned(), "students".to_owned(), tx));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
    });

    *cluster
        .latest_session()
        .page_delay_once
        .lock()
        .expect("page_delay_once") = Some(Duration::from_millis(250));
    rt_worker.update(|s, _| {
        s.reload_active_tab();
        assert!(
            s.active_session()
                .and_then(SessionState::active_table_tab)
                .is_some_and(|t| t.page_loading)
        );
        s.toggle_delete_row(RowKey::Existing(0));
    });
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading)
    });
    let tab_after = rt_worker
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab");
    assert_eq!(tab_after.staged.delete_count(), 1);

    // 7. Discarding an edit during an in-flight page fetch accepts the latest response.
    rt_worker.update(|s, _| s.discard_staged_changes());
    *cluster
        .latest_session()
        .page_delay_once
        .lock()
        .expect("page_delay_once") = Some(Duration::from_millis(200));
    rt_worker.update(|s, _| {
        s.set_active_tab_page_size(100);
        s.toggle_delete_row(RowKey::Existing(0));
        s.discard_staged_changes();
    });
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading)
    });
    let page_after_discard = rt_worker
        .state
        .active_session()
        .expect("session")
        .active_table_tab()
        .expect("table tab")
        .page
        .as_ref()
        .expect("updated page");
    assert_eq!(page_after_discard.limit, 100);

    // 7b. Deferred TablePageLoaded received while edits are staged is applied once edits are cleared.
    *cluster
        .latest_session()
        .page_delay_once
        .lock()
        .expect("page_delay_once") = Some(Duration::from_millis(200));
    rt_worker.update(|s, _| s.set_active_tab_page_size(50));
    rt_worker.update(|s, _| s.toggle_delete_row(RowKey::Existing(0)));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading)
    });
    {
        let tab = rt_worker
            .state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab");
        assert_eq!(tab.page.as_ref().expect("old page").limit, 100);
        assert_eq!(tab.staged.delete_count(), 1);
        assert_eq!(tab.pending_page.as_ref().expect("deferred page").limit, 50);
    }
    rt_worker.update(|s, _| s.discard_staged_changes());
    {
        let tab = rt_worker
            .state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab");
        assert_eq!(tab.page.as_ref().expect("applied deferred page").limit, 50);
        assert!(tab.pending_page.is_none());
    }

    *cluster
        .latest_session()
        .page_delay_once
        .lock()
        .expect("page_delay_once") = Some(Duration::from_millis(200));
    rt_worker.update(|s, _| s.set_active_tab_page_size(25));
    rt_worker.update(|s, _| s.toggle_delete_row(RowKey::Existing(0)));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading)
    });
    rt_worker.update(|s, _| s.toggle_delete_row(RowKey::Existing(0)));
    {
        let tab = rt_worker
            .state
            .active_session()
            .expect("session")
            .active_table_tab()
            .expect("table tab");
        assert_eq!(tab.staged.delete_count(), 0);
        assert_eq!(tab.page.as_ref().expect("applied deferred page on undo").limit, 25);
        assert!(tab.pending_page.is_none());
    }

    // 8. Stale ColumnsLoaded response from previous database does not overwrite current metadata.
    let stale_sess = cluster.latest_session();
    *stale_sess.columns_delay_once.lock().expect("columns_delay_once") =
        Some(Duration::from_millis(250));
    *stale_sess.columns_override.lock().expect("columns_override") = Some(vec![
        tv_core::ColumnSummary {
            name: "stale_db_a_col".to_owned(),
            data_type: "text".to_owned(),
            nullable: false,
            is_primary_key: false,
        },
    ]);
    rt_worker.update(|s, _| {
        s.toggle_object_expand("public".to_owned(), "students".to_owned());
    });
    std::thread::sleep(Duration::from_millis(20));
    rt_worker.update(|s, tx| {
        s.select_database("greendale_idp_dev".to_owned(), tx);
    });
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|sess| {
            sess.pending_db_switch_req.is_none()
                && sess
                    .status
                    .as_connected()
                    .and_then(|c| c.active_database.as_deref())
                    == Some("greendale_idp_dev")
        })
    });
    rt_worker.update(|s, _| {
        s.toggle_object_expand("public".to_owned(), "students".to_owned());
    });
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(|sess| sess.status.as_connected())
            .and_then(|c| c.expanded_columns.get(&("public".to_owned(), "students".to_owned())))
            .is_some_and(|st| matches!(st, tv_app::state::ColumnLoadState::Loaded(_)))
    });
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(50));
        rt_worker.frame(0.05);
    }
    let final_col_state = rt_worker
        .state
        .active_session()
        .and_then(|sess| sess.status.as_connected())
        .and_then(|c| c.expanded_columns.get(&("public".to_owned(), "students".to_owned())))
        .expect("expanded columns present");
    match final_col_state {
        tv_app::state::ColumnLoadState::Loaded(cols) => {
            assert!(cols.iter().any(|c| c.name == "student_id"));
            assert!(!cols.iter().any(|c| c.name == "stale_db_a_col"));
        }
        other => panic!("expected DB B loaded columns, got {other:?}"),
    }

    // 9. Late workspace refresh completing after disconnect must not delete table tabs with staged edits.
    rt_worker.update(|s, tx| s.select_object("public".to_owned(), "students".to_owned(), tx));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
    });
    rt_worker.update(|s, _| s.toggle_delete_row(RowKey::Existing(0)));
    let race_sess = cluster.latest_session();
    let objects_before = race_sess.objects_calls.load(Ordering::Acquire);
    *race_sess
        .objects_delay_once
        .lock()
        .expect("objects_delay_once") = Some(Duration::from_millis(250));
    rt_worker.update(|s, _| s.reload_workspace());
    std::thread::sleep(Duration::from_millis(30));
    race_sess.set_half_open(true);
    cluster.set_fail_connect(Some(DbError::AuthFailed("Access denied".to_owned())));
    rt_worker.update(|s, _| s.toggle_object_expand("public".to_owned(), "campus_news".to_owned()));
    poll_until(&mut rt_worker, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Disconnected { .. }))
    });
    poll_until(&mut rt_worker, Duration::from_secs(3), |_| {
        race_sess.objects_calls.load(Ordering::Acquire) > objects_before
    });
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(25));
        rt_worker.frame(0.05);
    }
    let race_tab = rt_worker
        .state
        .active_session()
        .expect("session still present")
        .active_table_tab()
        .expect("table tab must survive late refresh after disconnect");
    assert_eq!(race_tab.staged.delete_count(), 1);
}

#[test]
fn profile_edits_bind_active_session_and_guard_pending_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("connections.json");
    let store = ConnectionStore::at(store_path.clone());

    let mut prof_ro = ConnectionProfile::new_postgres("greendale_ro", "greendale-a.local", "troy_barnes");
    prof_ro.safety = SafetyPolicy::ReadOnly;
    prof_ro.env = EnvTag::Production;
    prof_ro.password_mode = tv_core::PasswordMode::Keychain;

    let mut prof_cw = ConnectionProfile::new_postgres("greendale_cw", "studya.local", "abed_nadir");
    prof_cw.safety = SafetyPolicy::ConfirmWrites;
    prof_cw.env = EnvTag::Production;
    prof_cw.password_mode = tv_core::PasswordMode::Keychain;

    let initial_store = StoreData {
        connections: vec![prof_ro.clone(), prof_cw.clone()],
        ..StoreData::default()
    };
    store.save(&initial_store).expect("seed store");

    let cluster = MockCluster::new(Duration::ZERO);
    let cred_backend = Arc::new(MockCredentialBackend::new());
    cred_backend.secrets.lock().expect("secrets").insert(
        prof_ro.credential_service(),
        ("troy_barnes".to_owned(), "human_beings".to_owned()),
    );
    cred_backend.secrets.lock().expect("secrets").insert(
        prof_cw.credential_service(),
        ("abed_nadir".to_owned(), "cool_cool_cool".to_owned()),
    );
    let cred_trait: Arc<dyn CredentialBackend> = cred_backend.clone();

    let mut state = AppState::init_in_memory(initial_store);
    state.persist_enabled = true;
    state.store_path = store_path;
    state.appearance = ColorScheme::Dark;
    state.worker = Some(
        WorkerBridge::spawn_with_connector_and_credentials(cluster.connector(), cred_trait)
            .expect("spawn worker"),
    );
    let mut rt = Runtime::new(state, views::view, (1280.0, 800.0), Env::new(true, 1.0));
    rt.on_key = Some(tv_app::platform::handle_global_key);
    rt.poll = Some(AppState::poll);

    // 1. Connect to greendale_ro on greendale-a.local (ReadOnly, Production).
    let ro_id = prof_ro.id;
    let cw_id = prof_cw.id;
    rt.update(|s, tx| s.start_or_focus_connection(ro_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| matches!(sess.status, ConnectionState::Connected(_)))
    });
    assert_eq!(cluster.connect_hosts(), vec!["greendale-a.local".to_owned()]);
    let gen_ro = rt
        .state
        .active_session()
        .expect("ro session")
        .active_connect_req_id;

    // 2. Edit greendale_ro to greendale-b.local + Unrestricted + Local and Save (without connecting).
    rt.update(|s, tx| s.open_edit_connection_form(ro_id, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        matches!(
            &rt.state.overlay,
            OverlayState::ConnectionForm(f) if !f.loading_credential
        )
    });
    rt.update(|s, tx| {
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.host = rune::TextEdit::new("greendale-b.local");
            form.password = rune::TextEdit::new("new_host_b_secret");
            form.password_touched = true;
            form.safety = SafetyPolicy::Unrestricted;
            form.env = EnvTag::Local;
        }
        s.save_connection_form(false, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    rt.frame(0.1);
    assert!(rt.find("toolbar-apply-profile").is_some());
    {
        let sess = rt.state.active_session().expect("ro session");
        assert_eq!(sess.profile.host, "greendale-a.local");
        assert_eq!(sess.profile.safety, SafetyPolicy::ReadOnly);
        assert_eq!(sess.profile.env, EnvTag::Production);
        assert!(sess.profile_out_of_sync);
    }

    // Active ReadOnly policy remains enforced on greendale-a.local even after profile edit.
    rt.update(|s, tx| s.select_object("public".to_owned(), "students".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
    });
    rt.update(|s, tx| {
        assert!(!s.insert_staged_row(tx));
        assert!(
            s.status_notice
                .as_ref()
                .is_some_and(|n| n.message.contains("Read-Only"))
        );
        s.open_new_query_tab(tx);
        if let Some(q) = s.active_session_mut().and_then(SessionState::active_query_tab_mut) {
            q.editor = rune::TextEdit::new("DELETE FROM public.students;");
        }
        s.run_active_query(true);
    });
    rt.frame(0.1);
    assert!(
        rt.state
            .active_session()
            .and_then(SessionState::active_query_tab)
            .and_then(|q| q.batch_error.as_deref())
            .is_some_and(|e| e.contains("Read-Only"))
    );

    // 3. Silent worker reconnect after half-open disconnect MUST stay on greendale-a.local.
    let ro_mock = cluster.latest_session();
    ro_mock.set_disconnected(true);
    rt.update(|s, tx| {
        s.select_object("public".to_owned(), "students".to_owned(), tx);
        s.reload_active_tab();
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
            && cluster.connect_hosts().len() >= 2
    });
    assert_eq!(
        cluster.connect_hosts().last().map(String::as_str),
        Some("greendale-a.local")
    );
    assert_eq!(
        rt.state
            .active_session()
            .expect("ro session")
            .active_connect_req_id,
        gen_ro
    );

    // 3b. Switching database in the existing session uses its bound host and credentials,
    // NOT the newly saved Keychain password meant for greendale-b.local.
    rt.update(|s, tx| s.select_database("greendale_idp_dev".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|sess| {
            sess.pending_db_switch_req.is_none()
                && sess
                    .status
                    .as_connected()
                    .and_then(|c| c.active_database.as_deref())
                    == Some("greendale_idp_dev")
        })
    });
    assert_eq!(
        cluster.connect_hosts().last().map(String::as_str),
        Some("greendale-a.local")
    );
    assert_eq!(
        cluster
            .connect_calls()
            .last()
            .and_then(|(pw, _)| pw.as_deref()),
        Some("human_beings")
    );

    // 3c. Reopening a disconnected workspace keeps it bound to greendale-a.local and preserves unsaved SQL.
    let unsaved_q_tab = {
        rt.update(|s, tx| {
            s.open_new_query_tab(tx);
            if let Some(q) = s.active_session_mut().and_then(SessionState::active_query_tab_mut) {
                q.editor = rune::TextEdit::new("SELECT 'unsaved_sql';");
            }
            s.disconnect_active(tx);
            s.start_or_focus_connection(ro_id, None, None, tx);
        });
        assert!(matches!(
            &rt.state.overlay,
            OverlayState::PasswordPrompt(p) if p.error.as_deref().is_some_and(|e| e.contains("Profile settings changed"))
        ));
        let connects_before_empty_submit = cluster.connect_count();
        rt.update(|s, tx| s.submit_password_prompt(tx));
        assert_eq!(cluster.connect_count(), connects_before_empty_submit);
        assert!(matches!(
            &rt.state.overlay,
            OverlayState::PasswordPrompt(p) if p.error.as_deref().is_some_and(|e| e.contains("Enter a password"))
        ));
        let sess = rt.state.active_session().expect("ro session");
        assert_eq!(sess.profile.host, "greendale-a.local");
        let q = sess.active_query_tab().expect("preserved query tab");
        assert_eq!(q.editor.text, "SELECT 'unsaved_sql';");
        q.tab_id
    };
    assert!(
        rt.state
            .active_session()
            .expect("ro session")
            .tabs
            .iter()
            .any(|t| t.id == unsaved_q_tab)
    );
    rt.update(|s, _| s.overlay = OverlayState::None);

    // 4. Switch to editable profile greendale_cw on studya.local (ConfirmWrites, Production).
    rt.update(|s, tx| s.start_or_focus_connection(cw_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .is_some_and(|sess| sess.profile.id == cw_id && matches!(sess.status, ConnectionState::Connected(_)))
    });
    assert_eq!(
        cluster.connect_hosts().last().map(String::as_str),
        Some("studya.local")
    );
    let gen_cw = rt
        .state
        .active_session()
        .expect("cw session")
        .active_connect_req_id;

    rt.update(|s, tx| s.select_object("public".to_owned(), "students".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
    });
    let cw_table_tab = rt
        .state
        .active_session()
        .expect("cw session")
        .active_tab
        .expect("table tab id");
    rt.update(|s, _| s.toggle_delete_row(RowKey::Existing(0)));

    // Cosmetic rename updates session.profile.name in-place without marking out_of_sync or clearing tabs.
    rt.update(|s, tx| s.open_edit_connection_form(cw_id, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        matches!(
            &rt.state.overlay,
            OverlayState::ConnectionForm(f) if !f.loading_credential
        )
    });
    rt.update(|s, tx| {
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.name = rune::TextEdit::new("greendale_cw_renamed");
        }
        s.save_connection_form(false, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    {
        let sess = rt.state.active_session().expect("cw session");
        assert_eq!(sess.profile.name, "greendale_cw_renamed");
        assert!(!sess.profile_out_of_sync);
        assert_eq!(sess.active_connect_req_id, gen_cw);
        assert_eq!(
            sess.active_table_tab().expect("table tab").staged.delete_count(),
            1
        );
    }

    // Capture a SafetyConfirmModalState bound to gen_cw before switching target.
    let stale_safety_overlay = {
        rt.update(|s, tx| {
            s.open_new_query_tab(tx);
            if let Some(q) = s.active_session_mut().and_then(SessionState::active_query_tab_mut) {
                q.editor = rune::TextEdit::new("DELETE FROM public.students;");
            }
            s.run_active_query(true);
        });
        let captured = match &rt.state.overlay {
            OverlayState::SafetyConfirm(m) => {
                let mut cloned = (**m).clone();
                cloned.destructive_ack = true;
                cloned
            }
            other => panic!("expected SafetyConfirm modal, got {other:?}"),
        };
        assert_eq!(captured.connect_req_id, gen_cw);
        rt.update(|s, _| s.overlay = OverlayState::None);
        captured
    };

    // Edit greendale_cw to studyb.local + Unrestricted + Local and click Save & Connect while staged edits exist.
    rt.update(|s, tx| s.open_edit_connection_form(cw_id, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        matches!(
            &rt.state.overlay,
            OverlayState::ConnectionForm(f) if !f.loading_credential
        )
    });
    rt.update(|s, tx| {
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.host = rune::TextEdit::new("studyb.local");
            form.safety = SafetyPolicy::Unrestricted;
            form.env = EnvTag::Local;
        }
        s.save_connection_form(true, tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    {
        let sess = rt.state.active_session().expect("cw session");
        assert_eq!(sess.profile.host, "studya.local");
        assert_eq!(sess.active_connect_req_id, gen_cw);
        assert!(sess.profile_out_of_sync);
        assert_eq!(sess.active_tab, Some(cw_table_tab));
        let tab = sess.active_table_tab().expect("dirty table tab");
        assert_eq!(tab.staged.delete_count(), 1);
        assert!(tab.diff_drawer_open);
        assert!(
            rt.state
                .status_notice
                .as_ref()
                .is_some_and(|n| n.message.contains("staged edits"))
        );
    }

    // Apply Profile remains blocked until staged edits are resolved.
    rt.update(|s, tx| s.apply_updated_profile_to_active_session(tx));
    assert_eq!(
        rt.state.active_session().expect("cw session").profile.host,
        "studya.local"
    );

    // Discard staged edits and apply the updated profile -> fresh generation, studyb.local, clean tabs.
    rt.update(|s, tx| {
        s.discard_staged_changes();
        s.apply_updated_profile_to_active_session(tx);
    });
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|sess| {
            sess.active_connect_req_id != gen_cw
                && matches!(sess.status, ConnectionState::Connected(_))
        })
    });
    let studyb_mock = cluster.latest_session();
    {
        let sess = rt.state.active_session().expect("rebound cw session");
        assert_ne!(sess.active_connect_req_id, gen_cw);
        assert_eq!(sess.profile.host, "studyb.local");
        assert_eq!(sess.profile.safety, SafetyPolicy::Unrestricted);
        assert!(!sess.profile_out_of_sync);
        assert!(sess.tabs.is_empty());
        assert_eq!(
            cluster.connect_hosts().last().map(String::as_str),
            Some("studyb.local")
        );
    }

    // Stale SafetyConfirm modal captured under gen_cw is rejected after connection generation changed.
    rt.update(|s, _| {
        s.overlay = OverlayState::SafetyConfirm(Box::new(stale_safety_overlay));
        s.confirm_safety_modal();
    });
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.message.contains("Connection or safety policy changed")),
        "expected stale confirmation notice, got {:?}",
        rt.state.status_notice
    );
    assert_eq!(studyb_mock.sql_calls.load(Ordering::Acquire), 0);

    // 5. Passwordless Keychain session + pending save must not send newly saved password to old host.
    let mut prof_nokey =
        ConnectionProfile::new_postgres("greendale_nokey", "host-a.greendale.test", "chang");
    prof_nokey.password_mode = tv_core::PasswordMode::Keychain;
    let nokey_id = prof_nokey.id;
    rt.update(|s, _| {
        s.connections.push(prof_nokey.clone());
    });
    let mut updated_store = store.load().expect("reload store");
    updated_store.connections.push(prof_nokey);
    store.save(&updated_store).expect("save store with nokey");

    rt.update(|s, tx| s.start_or_focus_connection(nokey_id, None, None, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.active_session().is_some_and(|sess| {
            sess.profile.id == nokey_id && matches!(sess.status, ConnectionState::Connected(_))
        })
    });
    assert_eq!(
        cluster.connect_hosts().last().map(String::as_str),
        Some("host-a.greendale.test")
    );
    assert_eq!(
        cluster
            .connect_calls()
            .last()
            .and_then(|(pw, _)| pw.as_deref()),
        None
    );

    // Slow keychain write keeps pending_profile_save active; manual reconnect must be refused.
    rt.update(|s, tx| s.open_edit_connection_form(nokey_id, tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        matches!(
            &rt.state.overlay,
            OverlayState::ConnectionForm(f) if !f.loading_credential
        )
    });
    *cred_backend.write_delay.lock().expect("write_delay") = Duration::from_millis(250);
    let connects_before_pending = cluster.connect_count();
    rt.update(|s, tx| {
        if let OverlayState::ConnectionForm(ref mut form) = s.overlay {
            form.host = rune::TextEdit::new("host-b.greendale.test");
            form.password = rune::TextEdit::new("new_host_b_secret");
            form.password_touched = true;
        }
        s.save_connection_form(false, tx);
    });
    assert!(rt.state.pending_profile_save.is_some());
    rt.update(|s, tx| s.reconnect_active(tx));
    assert_eq!(cluster.connect_count(), connects_before_pending);
    assert!(
        rt.state
            .status_notice
            .as_ref()
            .is_some_and(|n| n.message.contains("pending profile save"))
    );

    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state.pending_profile_save.is_none()
    });
    *cred_backend.write_delay.lock().expect("write_delay") = Duration::ZERO;

    // Background reconnect after half-open disconnect keeps original host and original password (None).
    let nokey_mock = cluster.latest_session();
    nokey_mock.set_disconnected(true);
    rt.update(|s, tx| s.select_object("public".to_owned(), "students".to_owned(), tx));
    poll_until(&mut rt, Duration::from_secs(3), |rt| {
        rt.state
            .active_session()
            .and_then(SessionState::active_table_tab)
            .is_some_and(|t| !t.page_loading && t.page.is_some())
            && cluster.connect_count() > connects_before_pending
    });
    assert_eq!(
        cluster.connect_hosts().last().map(String::as_str),
        Some("host-a.greendale.test")
    );
    assert_eq!(
        cluster
            .connect_calls()
            .last()
            .and_then(|(pw, _)| pw.as_deref()),
        None
    );
}
