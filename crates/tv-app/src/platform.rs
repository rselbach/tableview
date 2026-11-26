use rune::{ColorScheme, Key, KeyEvent, MenuItem, Shortcut, Tx};

use crate::state::{AppState, OverlayState, PickerKind, Screen};

/// Logical user actions triggered by shortcuts, native menus, or toolbar controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewConnection,
    ImportUrl,
    EditActiveConnection,
    OpenConnectionSwitcher,
    OpenDatabasePicker,
    QuickJumpObject,
    NewQueryTab,
    RunCurrentQuery,
    RunAllQueries,
    CancelQuery,
    FormatSql,
    SaveQuery,
    ToggleSidebar,
    ToggleConsole,
    ToggleInspector,
    ReloadWorkspace,
    ReloadActiveTab,
    CloseActiveTab,
    PrevTab,
    NextTab,
    FocusFilter,
    Reconnect,
    Disconnect,
    ShowLauncher,
    SetColorScheme(ColorScheme),
    CycleColorScheme,
    DismissOverlay,
}

/// Platform-aware primary modifier glyph (`⌘` on macOS, `Ctrl+` elsewhere).
pub const fn primary_prefix() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" }
}

fn is_primary_enter(ev: &KeyEvent) -> bool {
    if ev.key != Key::Enter || ev.mods.alt {
        return false;
    }
    if cfg!(target_os = "macos") {
        ev.mods.cmd && !ev.mods.ctrl
    } else {
        ev.mods.ctrl && !ev.mods.cmd
    }
}

/// Translates a `KeyEvent` into a TableView [`Action`].
pub fn action_for_key(ev: &KeyEvent) -> Option<Action> {
    if ev.key == Key::Escape {
        return Some(Action::DismissOverlay);
    }
    if is_primary_enter(ev) {
        return Some(if ev.mods.shift {
            Action::RunAllQueries
        } else {
            Action::RunCurrentQuery
        });
    }
    if ev.is_command('t') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::NewQueryTab);
    }
    if ev.is_command('s') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::SaveQuery);
    }
    if ev.is_command('k') {
        return Some(if ev.mods.shift {
            Action::OpenConnectionSwitcher
        } else if !ev.mods.alt && !ev.mods.ctrl {
            Action::OpenDatabasePicker
        } else {
            return None;
        });
    }
    if ev.is_command('p') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::QuickJumpObject);
    }
    if ev.is_command('r') {
        if ev.mods.shift && !ev.mods.alt {
            return Some(Action::Reconnect);
        }
        if ev.mods.alt && !ev.mods.shift {
            return Some(Action::ReloadActiveTab);
        }
        if !ev.mods.shift && !ev.mods.alt {
            return Some(Action::ReloadWorkspace);
        }
        return None;
    }
    if ev.is_command('w') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::CloseActiveTab);
    }
    if (ev.is_command('[') || ev.is_command('{')) && ev.mods.shift && !ev.mods.alt {
        return Some(Action::PrevTab);
    }
    if (ev.is_command(']') || ev.is_command('}')) && ev.mods.shift && !ev.mods.alt {
        return Some(Action::NextTab);
    }
    if ev.is_command('f') {
        if ev.mods.shift && !ev.mods.alt {
            return Some(Action::FormatSql);
        }
        if !ev.mods.shift && !ev.mods.alt {
            return Some(Action::FocusFilter);
        }
        return None;
    }
    if ev.is_command('.') {
        if ev.mods.alt {
            return Some(Action::Disconnect);
        }
        if !ev.mods.shift && !ev.mods.ctrl {
            return Some(Action::CancelQuery);
        }
        return None;
    }
    if ev.is_command('n') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::NewConnection);
    }
    if ev.is_command('i') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::ImportUrl);
    }
    if ev.is_command('1') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::ToggleSidebar);
    }
    if ev.is_command('b') && ev.mods.shift && !ev.mods.alt {
        return Some(Action::ToggleSidebar);
    }
    if ev.is_command('2') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::ToggleConsole);
    }
    if ev.is_command('3') && !ev.mods.shift && !ev.mods.alt {
        return Some(Action::ToggleInspector);
    }
    None
}

/// Executes a logical action against [`AppState`]. Returns `true` if handled/consumed.
pub fn dispatch_action(s: &mut AppState, action: Action, tx: &mut Tx) -> bool {
    match action {
        Action::DismissOverlay => {
            if !matches!(s.overlay, OverlayState::None) {
                s.overlay = OverlayState::None;
                return true;
            }
            if s.launcher_context_menu.is_some() {
                s.launcher_context_menu = None;
                return true;
            }
            if let Some(session) = s.active_session_mut() {
                if session.schema_dropdown_open {
                    session.schema_dropdown_open = false;
                    return true;
                }
                if let Some(tab) = session.active_table_tab_mut() {
                    if tab.header_menu.is_some() {
                        tab.header_menu = None;
                        return true;
                    }
                    if tab.page_size_open {
                        tab.page_size_open = false;
                        return true;
                    }
                }
                if let Some(q) = session.active_query_tab_mut()
                    && q.limit_dropdown_open
                {
                    q.limit_dropdown_open = false;
                    return true;
                }
            }
            false
        }
        Action::NewConnection => {
            s.open_new_connection_form(tx);
            true
        }
        Action::ImportUrl => {
            s.open_import_url_dialog(tx);
            true
        }
        Action::EditActiveConnection => {
            if let Some(session) = s.active_session() {
                let id = session.profile.id;
                s.open_edit_connection_form(id, tx);
                true
            } else if let Some(id) = s.launcher_selected {
                s.open_edit_connection_form(id, tx);
                true
            } else {
                false
            }
        }
        Action::OpenConnectionSwitcher => {
            s.open_picker(PickerKind::Connection, tx);
            true
        }
        Action::OpenDatabasePicker => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.open_picker(PickerKind::Database, tx);
                true
            } else {
                false
            }
        }
        Action::QuickJumpObject => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.open_picker(PickerKind::QuickJump, tx);
                true
            } else {
                false
            }
        }
        Action::NewQueryTab => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.open_new_query_tab(tx);
                true
            } else {
                false
            }
        }
        Action::RunCurrentQuery => s.run_active_query(false),
        Action::RunAllQueries => s.run_active_query(true),
        Action::CancelQuery => s.cancel_active_query(),
        Action::FormatSql => s.format_active_query_sql(),
        Action::SaveQuery => s.open_save_query_modal(tx),
        Action::ToggleSidebar => {
            if let Some(session) = s.active_session_mut() {
                session.sidebar_pane.toggle();
                true
            } else {
                false
            }
        }
        Action::ToggleConsole => {
            if let Some(session) = s.active_session_mut() {
                session.console_open = !session.console_open;
                true
            } else {
                false
            }
        }
        Action::ToggleInspector => {
            if let Some(session) = s.active_session_mut() {
                session.inspector_pane.toggle();
                true
            } else {
                false
            }
        }
        Action::ReloadWorkspace => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.reload_workspace();
                true
            } else {
                false
            }
        }
        Action::ReloadActiveTab => {
            if s.active_session().and_then(|sess| sess.active_tab).is_some() {
                s.reload_active_tab();
                true
            } else {
                false
            }
        }
        Action::CloseActiveTab => s.close_active_tab(tx),
        Action::PrevTab => s.cycle_tab(-1, tx),
        Action::NextTab => s.cycle_tab(1, tx),
        Action::FocusFilter => {
            if matches!(
                s.active_session().and_then(|sess| sess.active_table_tab()),
                Some(t) if t.view_mode == crate::state::TableViewMode::Data
            ) {
                tx.focus("table-filter-input", true);
                true
            } else {
                false
            }
        }
        Action::Reconnect => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.reconnect_active(tx);
                true
            } else {
                false
            }
        }
        Action::Disconnect => {
            if matches!(s.screen, Screen::Workspace(_)) {
                s.disconnect_active(tx);
                true
            } else {
                false
            }
        }
        Action::ShowLauncher => {
            s.overlay = OverlayState::None;
            s.screen = Screen::Launcher;
            s.sync_window_title(tx);
            true
        }
        Action::SetColorScheme(scheme) => {
            s.appearance = scheme;
            tx.set_color_scheme(scheme);
            true
        }
        Action::CycleColorScheme => {
            let next = match s.appearance {
                ColorScheme::System => ColorScheme::Dark,
                ColorScheme::Dark => ColorScheme::Light,
                ColorScheme::Light => ColorScheme::System,
            };
            s.appearance = next;
            tx.set_color_scheme(next);
            true
        }
    }
}

/// Global keyboard handler shared between root `on_key_preview` and `AppSpec::on_key`.
pub fn handle_global_key(s: &mut AppState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if !matches!(s.overlay, OverlayState::None) {
        return false;
    }
    if let Some(action) = action_for_key(ev) {
        return dispatch_action(s, action, tx);
    }
    false
}

fn shortcut(spec: &str) -> Option<Shortcut> {
    spec.parse::<Shortcut>().ok()
}

fn item_with_shortcut(
    id: &str,
    title: &str,
    menu: &str,
    chord: &str,
    action: fn(&mut AppState, &mut Tx),
) -> MenuItem<AppState> {
    let mut item = MenuItem::new(title, action).id(id).menu(menu).keys_first();
    if let Some(sc) = shortcut(chord) {
        item = item.key_equivalent(sc);
    }
    item
}

/// Builds native macOS menu items (`Connection`, `View`, `Theme`) mapped to TableView actions.
pub fn native_menu_items() -> Vec<MenuItem<AppState>> {
    vec![
        MenuItem::about("About TableView", |s: &mut AppState, tx| {
            s.set_notice("TableView 0.1.0 — PostgreSQL client built with Rune", false);
            s.sync_window_title(tx);
        })
        .id("tv-about"),
        item_with_shortcut("conn-new", "New Connection…", "Connection", "mod+n", |s, tx| {
            dispatch_action(s, Action::NewConnection, tx);
        }),
        item_with_shortcut("conn-import", "Import URL…", "Connection", "mod+i", |s, tx| {
            dispatch_action(s, Action::ImportUrl, tx);
        }),
        MenuItem::new("Edit Connection…", |s, tx| {
            dispatch_action(s, Action::EditActiveConnection, tx);
        })
        .id("conn-edit")
        .menu("Connection"),
        MenuItem::separator().menu("Connection"),
        item_with_shortcut("conn-open-db", "Open a Database…", "Connection", "mod+k", |s, tx| {
            dispatch_action(s, Action::OpenDatabasePicker, tx);
        }),
        item_with_shortcut(
            "conn-open-conn",
            "Open a Connection…",
            "Connection",
            "mod+shift+k",
            |s, tx| {
                dispatch_action(s, Action::OpenConnectionSwitcher, tx);
            },
        ),
        item_with_shortcut("conn-quick-jump", "Jump to Table…", "Connection", "mod+p", |s, tx| {
            dispatch_action(s, Action::QuickJumpObject, tx);
        }),
        MenuItem::separator().menu("Connection"),
        item_with_shortcut("query-new", "New SQL Query", "Connection", "mod+t", |s, tx| {
            dispatch_action(s, Action::NewQueryTab, tx);
        }),
        item_with_shortcut(
            "query-run-current",
            "Run Current Query",
            "Connection",
            "mod+enter",
            |s, tx| {
                dispatch_action(s, Action::RunCurrentQuery, tx);
            },
        ),
        item_with_shortcut(
            "query-run-all",
            "Run All Queries",
            "Connection",
            "mod+shift+enter",
            |s, tx| {
                dispatch_action(s, Action::RunAllQueries, tx);
            },
        ),
        item_with_shortcut("query-cancel", "Cancel Running Query", "Connection", "mod+period", |s, tx| {
            dispatch_action(s, Action::CancelQuery, tx);
        }),
        item_with_shortcut("query-format", "Format SQL", "Connection", "mod+shift+f", |s, tx| {
            dispatch_action(s, Action::FormatSql, tx);
        }),
        item_with_shortcut("query-save", "Save Query…", "Connection", "mod+s", |s, tx| {
            dispatch_action(s, Action::SaveQuery, tx);
        }),
        MenuItem::new("Connection Launcher", |s, tx| {
            dispatch_action(s, Action::ShowLauncher, tx);
        })
        .id("conn-launcher")
        .menu("Connection"),
        MenuItem::separator().menu("Connection"),
        item_with_shortcut("conn-reload", "Reload Workspace", "Connection", "mod+r", |s, tx| {
            dispatch_action(s, Action::ReloadWorkspace, tx);
        }),
        item_with_shortcut("conn-reload-tab", "Reload Current Tab", "Connection", "mod+alt+r", |s, tx| {
            dispatch_action(s, Action::ReloadActiveTab, tx);
        }),
        item_with_shortcut("conn-close-tab", "Close Tab", "Connection", "mod+w", |s, tx| {
            dispatch_action(s, Action::CloseActiveTab, tx);
        }),
        item_with_shortcut("conn-reconnect", "Reconnect", "Connection", "mod+shift+r", |s, tx| {
            dispatch_action(s, Action::Reconnect, tx);
        }),
        item_with_shortcut("conn-disconnect", "Disconnect", "Connection", "mod+alt+period", |s, tx| {
            dispatch_action(s, Action::Disconnect, tx);
        }),
        item_with_shortcut("nav-filter", "Filter Table…", "Navigate", "mod+f", |s, tx| {
            dispatch_action(s, Action::FocusFilter, tx);
        }),
        MenuItem::new("Previous Tab", |s, tx| {
            dispatch_action(s, Action::PrevTab, tx);
        })
        .id("nav-prev-tab")
        .menu("Navigate"),
        MenuItem::new("Next Tab", |s, tx| {
            dispatch_action(s, Action::NextTab, tx);
        })
        .id("nav-next-tab")
        .menu("Navigate"),
        item_with_shortcut("view-sidebar", "Toggle Sidebar", "View", "mod+1", |s, tx| {
            dispatch_action(s, Action::ToggleSidebar, tx);
        }),
        item_with_shortcut("view-console", "Toggle Console Log", "View", "mod+2", |s, tx| {
            dispatch_action(s, Action::ToggleConsole, tx);
        }),
        item_with_shortcut("view-inspector", "Toggle Inspector", "View", "mod+3", |s, tx| {
            dispatch_action(s, Action::ToggleInspector, tx);
        }),
        MenuItem::new("System", |s, tx| {
            dispatch_action(s, Action::SetColorScheme(ColorScheme::System), tx);
        })
        .id("theme-system")
        .menu("Theme"),
        MenuItem::new("Light", |s, tx| {
            dispatch_action(s, Action::SetColorScheme(ColorScheme::Light), tx);
        })
        .id("theme-light")
        .menu("Theme"),
        MenuItem::new("Dark", |s, tx| {
            dispatch_action(s, Action::SetColorScheme(ColorScheme::Dark), tx);
        })
        .id("theme-dark")
        .menu("Theme"),
    ]
}
