use rune::{Cx, El, ScrollSpec, Tx, col, el, icon, rich, row, spacer, text};
use rune_kit::shell::SidebarToggle;
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl, Select,
    SelectAppearance, Sidebar, Spinner, SplitView, TextInput, Toolbar,
};
use tv_core::{DbObject, DbObjectKind, SafetyPolicy};

use crate::icons;
use crate::platform::{self, Action};
use crate::state::{
    AppState, ColumnLoadState, ConnectedData, ConnectionState, PickerKind, SessionState, SidebarTab, TabContent,
    WorkspaceTab,
};
use crate::theme;
use crate::views::{empty_cta, inspector, pill_badge, table_grid};

/// Builds the full database workspace screen (`SplitView` + environment strip + console).
pub fn build(
    s: &AppState,
    session: &SessionState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let env_pal = match &session.status {
        ConnectionState::Disconnected { .. } | ConnectionState::Failed(_) => theme::disconnected_banner_colors(k.dark),
        _ => theme::env_colors(session.profile.env, session.profile.color, k.dark),
    };

    let sid = session.id;
    let toolbar = build_toolbar(s, session, k);
    let sidebar = build_sidebar(session, k, layer);
    let inspector_pane = inspector::build(session, k);
    let main = build_main_pane(s, session, k, cx, layer);

    let split = SplitView::new(main)
        .sidebar(
            &session.sidebar_pane,
            move |s: &mut AppState| s.sidebar_pane_mut(sid),
            sidebar,
        )
        .inspector(
            &session.inspector_pane,
            move |s: &mut AppState| s.inspector_pane_mut(sid),
            inspector_pane,
        )
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .flat_main(true)
        .sidebar_toggle(SidebarToggle::new().in_header().shortcut("mod+1"))
        .reduced_motion(cx.is_reduced_motion())
        .build(k);

    let accent_strip = el()
        .key("workspace-env-strip")
        .absolute(0.0, 0.0)
        .w_full()
        .h(2.5)
        .fill(env_pal.strip)
        .inert();

    el()
        .key("workspace-root")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(split)
        .child(accent_strip)
}

fn build_toolbar(s: &AppState, session: &SessionState, k: &Kit) -> Toolbar {
    let c = &k.c;
    let session_count = s.sessions.len();
    let conn_label = if session_count > 1 {
        format!("Connections ({session_count})")
    } else {
        "Connections".to_owned()
    };

    let has_connected = session.status.as_connected().is_some();

    let leading = row()
        .gap(4.0)
        .center_y()
        .child(
            Button::new(conn_label)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .icon(&icons::PLUG)
                .shortcut("mod+shift+k")
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_picker(PickerKind::Connection, tx))
                .build(k),
        )
        .child(
            IconButton::new(&icons::DATABASE)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Regular)
                .label("Open database")
                .help("Open database")
                .shortcut("mod+k")
                .disabled(!has_connected)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_picker(PickerKind::Database, tx))
                .build(k),
        );

    let status_pill = build_status_pill(session, k);

    let mut trailing = row().gap(4.0).center_y();
    if session.profile.safety != SafetyPolicy::Unrestricted {
        let short_safety = match session.profile.safety {
            SafetyPolicy::Unrestricted => "",
            SafetyPolicy::ConfirmWrites => "Confirm",
            SafetyPolicy::StagedOnly => "Staged",
            SafetyPolicy::ReadOnly => "Read-Only",
        };
        trailing = trailing.child(pill_badge(
            "toolbar/safety",
            short_safety,
            c.layer_02,
            c.text_muted,
            c.border_muted,
            k,
        ));
    }
    trailing = trailing
        .child(
            IconButton::new(&icons::REFRESH)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Regular)
                .label("Reload workspace")
                .help("Reload workspace")
                .shortcut("mod+r")
                .disabled(!has_connected)
                .on_press(|s: &mut AppState| s.reload_workspace())
                .build(k),
        )
        .child(
            IconButton::new(&icons::TERMINAL)
                .variant(if session.console_open {
                    IconButtonVariant::Ghost
                } else {
                    IconButtonVariant::GhostMuted
                })
                .control_size(ControlSize::Regular)
                .label("Toggle console log")
                .help("Toggle console log")
                .shortcut("mod+2")
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                    platform::dispatch_action(s, Action::ToggleConsole, tx);
                })
                .build(k),
        )
        .child(
            IconButton::new(&icons::INSPECTOR)
                .variant(if session.inspector_pane.is_open() {
                    IconButtonVariant::Ghost
                } else {
                    IconButtonVariant::GhostMuted
                })
                .control_size(ControlSize::Regular)
                .label("Toggle inspector")
                .help("Toggle inspector")
                .shortcut("mod+3")
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                    platform::dispatch_action(s, Action::ToggleInspector, tx);
                })
                .build(k),
        )
        .child(
            IconButton::new(&icons::SUN_MOON)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Regular)
                .label("Cycle theme")
                .help("Cycle theme")
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                    platform::dispatch_action(s, Action::CycleColorScheme, tx);
                })
                .build(k),
        );

    Toolbar::new()
        .leading(leading)
        .center(status_pill)
        .trailing(trailing)
}

fn build_status_pill(session: &SessionState, k: &Kit) -> El {
    let c = &k.c;
    let tag = session.profile.env.banner_tag();
    let env_pal = theme::env_colors(session.profile.env, session.profile.color, k.dark);

    match &session.status {
        ConnectionState::Disconnected { .. } | ConnectionState::Failed(_) => {
            let err_pal = theme::disconnected_banner_colors(k.dark);
            row()
                .key("toolbar/status-pill")
                .role("button")
                .label("Reconnect database")
                .focusable(true)
                .h(28.0)
                .px(12.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .fill(err_pal.pill_bg)
                .border(0.5, err_pal.pill_border)
                .hover(c.hover)
                .press(c.pressed)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.reconnect_active(tx))
                .child(el().size(7.0, 7.0).radius(3.5).fill(err_pal.dot))
                .child(
                    text(format!("{tag}  |  Database is disconnected — Click to reconnect"))
                        .font(k.ty.mono)
                        .color(err_pal.pill_fg)
                        .truncate(),
                )
        }
        ConnectionState::Connecting | ConnectionState::Reconnecting => row()
            .key("toolbar/status-pill")
            .h(28.0)
            .px(12.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.md)
            .fill(c.layer_02)
            .border(0.5, c.border_base)
            .child(Spinner::new().size(12.0).color(c.icon_muted).build(k))
            .child(
                text(format!("{tag}  |  Connecting to {}…", session.profile.name))
                    .font(k.ty.mono)
                    .color(c.text_muted)
                    .truncate(),
            ),
        ConnectionState::Connected(data) => {
            let mut summary = format!("{} : {}", data.server_version, session.profile.name);
            if let Some(db) = &data.active_database {
                summary.push_str(&format!(" : {db}"));
            }
            let active_obj = session
                .active_tab()
                .and_then(WorkspaceTab::schema_and_name)
                .or_else(|| data.selected_object.as_ref().map(|(s, o)| (s.as_str(), o.as_str())));
            if let Some((schema, obj)) = active_obj {
                summary.push_str(&format!(" : {schema}.{obj}"));
            }
            row()
                .key("toolbar/status-pill")
                .role("button")
                .label("Switch database")
                .focusable(true)
                .max_w(500.0)
                .h(28.0)
                .px(10.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .fill(c.layer_02)
                .border(0.5, c.border_base)
                .hover(c.hover)
                .press(c.pressed)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_picker(PickerKind::Database, tx))
                .child(pill_badge(
                    "toolbar/env-tag",
                    tag,
                    env_pal.pill_bg,
                    env_pal.pill_fg,
                    env_pal.pill_border,
                    k,
                ))
                .child(
                    text(summary)
                        .font(k.ty.mono)
                        .color(c.text_base)
                        .shrink()
                        .truncate(),
                )
        }
    }
}

fn build_sidebar(session: &SessionState, k: &Kit, layer: &mut Vec<El>) -> El {
    let c = &k.c;
    let tabs = SegmentedControl::new(&["Items", "Queries", "History"], Some(session.sidebar_tab.index()))
        .label("Sidebar section")
        .as_tabs()
        .width(None)
        .on_pick(|s: &mut AppState, idx, _| {
            if let Some(sess) = s.active_session_mut() {
                sess.sidebar_tab = SidebarTab::from_index(idx);
            }
        })
        .build(k);

    let search = TextInput::new(&session.sidebar_search)
        .label("Filter items")
        .placeholder("Search for item…")
        .leading_icon(k.glyph.search)
        .clear_button()
        .width(None)
        .on_edit(AppState::sidebar_search_mut)
        .build(k);

    let filter_btn = IconButton::new(&icons::FILTER)
        .variant(if session.show_system_schemas {
            IconButtonVariant::Ghost
        } else {
            IconButtonVariant::GhostMuted
        })
        .control_size(ControlSize::Regular)
        .label("Toggle system schemas")
        .help(if session.show_system_schemas {
            "Hide system schemas"
        } else {
            "Show system schemas"
        })
        .on_press(|s: &mut AppState| {
            if let Some(sess) = s.active_session_mut() {
                sess.show_system_schemas = !sess.show_system_schemas;
            }
        })
        .build(k);

    let search_row = row()
        .w_full()
        .gap(6.0)
        .center_y()
        .child(el().grow(1.0).min_w(0.0).child(search))
        .child(filter_btn);

    let mut sb = Sidebar::new()
        .label("Database objects")
        .header(tabs)
        .header(search_row);

    match session.sidebar_tab {
        SidebarTab::Items => match &session.status {
            ConnectionState::Disconnected { reason } => {
                sb = sb.child(sidebar_message(
                    reason.as_deref().unwrap_or("Database is disconnected"),
                    Some(("Reconnect", |s: &mut AppState, tx: &mut Tx| s.reconnect_active(tx))),
                    k,
                ));
            }
            ConnectionState::Failed(err) => {
                sb = sb.child(sidebar_message(
                    err,
                    Some(("Reconnect", |s: &mut AppState, tx: &mut Tx| s.reconnect_active(tx))),
                    k,
                ));
            }
            ConnectionState::Connecting | ConnectionState::Reconnecting => {
                sb = sb.child(
                    col()
                        .w_full()
                        .py(48.0)
                        .center()
                        .gap(10.0)
                        .child(Spinner::new().size(16.0).color(c.icon_muted).build(k))
                        .child(text("Connecting…").font(k.ty.ui).color(c.text_muted)),
                );
            }
            ConnectionState::Connected(data) => {
                if data.active_database.is_none() {
                    sb = sb.child(sidebar_message(
                        "No database selected, press ⌘K to select a database",
                        Some(("Open Database…", |s: &mut AppState, tx: &mut Tx| {
                            s.open_picker(PickerKind::Database, tx);
                        })),
                        k,
                    ));
                } else {
                    let filtered = session.filtered_objects();
                    if filtered.is_empty() {
                        sb = sb.child(sidebar_message("No tables or views in this schema", None, k));
                    } else {
                        for obj in filtered {
                            sb = sb.child(build_object_item(data, obj, k));
                        }
                    }

                    let schema_names: Vec<String> = session
                        .visible_schemas()
                        .into_iter()
                        .map(|sc| sc.name.clone())
                        .collect();
                    let schema_refs: Vec<&str> = schema_names.iter().map(String::as_str).collect();
                    let schema_select = Select::new("sidebar-schema", &schema_refs)
                        .label("Schema")
                        .appearance(SelectAppearance::Field)
                        .fill()
                        .current(data.active_schema.as_deref())
                        .placeholder("schema")
                        .open(session.schema_dropdown_open)
                        .on_toggle(|s: &mut AppState| {
                            if let Some(sess) = s.active_session_mut() {
                                sess.schema_dropdown_open = !sess.schema_dropdown_open;
                            }
                        })
                        .on_close(|s: &mut AppState| {
                            if let Some(sess) = s.active_session_mut() {
                                sess.schema_dropdown_open = false;
                            }
                        })
                        .on_choose(|s: &mut AppState, val: &str, _| {
                            s.select_schema(val.to_owned());
                        })
                        .build(k, layer);

                    let count = session.filtered_objects().len();
                    sb = sb.footer(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(el().grow(1.0).min_w(0.0).child(schema_select))
                            .child(text(format!("{count}")).font(k.ty.small).color(c.text_faint)),
                    );
                }
            }
        },
        SidebarTab::Queries => {
            sb = sb.child(sidebar_message(
                "Saved SQL queries arrive in Milestone 3.",
                None,
                k,
            ));
        }
        SidebarTab::History => {
            sb = sb.child(sidebar_message(
                "Query history arrives in Milestone 3.",
                None,
                k,
            ));
        }
    }

    sb.build(k)
}

type SidebarCta = (&'static str, fn(&mut AppState, &mut Tx));

fn sidebar_message(msg: &str, cta: Option<SidebarCta>, k: &Kit) -> El {
    let mut box_el = col()
        .w_full()
        .py(48.0)
        .px(16.0)
        .center()
        .gap(12.0)
        .child(text(msg).font(k.ty.ui).color(k.c.text_muted).wrap().w_full());
    if let Some((label, handler)) = cta {
        box_el = box_el.child(empty_cta(label, k, false, handler));
    }
    box_el
}

fn build_object_item(data: &ConnectedData, obj: &DbObject, k: &Kit) -> El {
    let c = &k.c;
    let key = (obj.schema.clone(), obj.name.clone());
    let selected = data.selected_object.as_ref() == Some(&key);
    let col_state = data.expanded_columns.get(&key);
    let expanded = col_state.is_some();

    let obj_icon = match obj.kind {
        DbObjectKind::View => &icons::VIEW_EYE,
        DbObjectKind::MaterializedView => &icons::MATERIALIZED_VIEW,
        _ => &icons::TABLE,
    };

    let schema_for_expand = obj.schema.clone();
    let name_for_expand = obj.name.clone();
    let disclosure = row()
        .key(format!("sidebar/obj/{}.{}/toggle", obj.schema, obj.name))
        .role("button")
        .label(format!("Expand {}", obj.name))
        .size(18.0, 18.0)
        .center()
        .radius(k.radius.xs)
        .hover(c.hover)
        .on_press(move |s: &mut AppState| {
            s.toggle_object_expand(schema_for_expand.clone(), name_for_expand.clone());
        })
        .child(
            icon(
                if expanded {
                    k.glyph.triangle_down
                } else {
                    k.glyph.triangle_right
                },
                12.0,
            )
            .color(c.icon_muted),
        );

    let schema_for_sel = obj.schema.clone();
    let name_for_sel = obj.name.clone();
    let item_row = row()
        .key(format!("sidebar/obj/{}.{}", obj.schema, obj.name))
        .role("listitem")
        .label(&obj.name)
        .focusable(true)
        .w_full()
        .h(26.0)
        .px(4.0)
        .gap(5.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(if selected { c.layer_03 } else { rune::Color::TRANSPARENT })
        .hover(c.hover)
        .press(c.pressed)
        .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
            s.select_object(schema_for_sel.clone(), name_for_sel.clone(), tx);
        })
        .child(disclosure)
        .child(icon(obj_icon, 13.0).color(if selected { c.text_accent } else { c.icon_muted }))
        .child(
            text(&obj.name)
                .font(if selected { k.ty.ui_medium } else { k.ty.ui_compact })
                .color(c.text_base)
                .grow(1.0)
                .shrink()
                .truncate(),
        )
        .child(text(obj.kind.badge()).font(k.ty.small).color(c.text_faint));

    let mut wrapper = col().w_full().gap(1.0).child(item_row);
    if let Some(state) = col_state {
        match state {
            ColumnLoadState::Loading => {
                wrapper = wrapper.child(
                    row()
                        .pl(28.0)
                        .h(22.0)
                        .gap(6.0)
                        .center_y()
                        .child(Spinner::new().size(10.0).color(c.icon_faint).build(k))
                        .child(text("Loading columns…").font(k.ty.small).color(c.text_faint)),
                );
            }
            ColumnLoadState::Error(err) => {
                wrapper = wrapper.child(
                    row()
                        .pl(28.0)
                        .py(2.0)
                        .child(text(err).font(k.ty.small).color(c.danger.fg).truncate()),
                );
            }
            ColumnLoadState::Loaded(cols) => {
                for col_info in cols {
                    let mut col_row = row()
                        .key(format!("sidebar/col/{}.{}/{}", obj.schema, obj.name, col_info.name))
                        .w_full()
                        .h(21.0)
                        .pl(28.0)
                        .pr(6.0)
                        .gap(6.0)
                        .center_y();
                    if col_info.is_primary_key {
                        col_row = col_row.child(icon(&icons::KEY_ICON, 11.0).color(c.warning.icon));
                    }
                    col_row = col_row
                        .child(
                            text(&col_info.name)
                                .font(k.ty.small)
                                .color(c.text_muted)
                                .grow(1.0)
                                .shrink()
                                .truncate(),
                        )
                        .child(text(&col_info.data_type).font(k.ty.small).color(c.text_faint).truncate());
                    wrapper = wrapper.child(col_row);
                }
            }
        }
    }
    wrapper
}

fn build_main_pane(
    s: &AppState,
    session: &SessionState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let stage = build_stage(s, session, k, cx, layer);
    let mut main = col().key("workspace-main-inner").w_full().h_full().min_h(0.0).child(stage);
    if session.console_open {
        main = main.child(build_console_drawer(session, k));
    }
    main
}

fn build_stage(
    s: &AppState,
    session: &SessionState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let c = &k.c;
    match &session.status {
        ConnectionState::Disconnected { reason } => col()
            .key("stage-disconnected")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(12.0)
            .padding(32.0)
            .child(icon(&icons::LOGO, 40.0).color(c.icon_faint))
            .child(text("Database is disconnected").font(k.ty.body_medium).color(c.text_base))
            .child(
                text(
                    reason
                        .as_deref()
                        .unwrap_or("Reconnect to inspect schemas, tables, and SQL logs."),
                )
                .font(k.ty.ui)
                .color(c.text_muted),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(empty_cta("Reconnect", k, true, |s, tx| s.reconnect_active(tx)))
                    .child(empty_cta("Launcher", k, false, |s, tx| {
                        platform::dispatch_action(s, Action::ShowLauncher, tx);
                    })),
            ),
        ConnectionState::Failed(err) => col()
            .key("stage-failed")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(12.0)
            .padding(32.0)
            .child(icon(&icons::PLUG, 36.0).color(c.danger.fg))
            .child(text("Connection failed").font(k.ty.body_medium).color(c.text_base))
            .child(text(err).font(k.ty.ui).color(c.danger.fg).wrap())
            .child(
                row()
                    .gap(8.0)
                    .child(empty_cta("Retry Connection", k, true, |s, tx| s.reconnect_active(tx)))
                    .child(empty_cta("Edit Connection…", k, false, |s, tx| {
                        platform::dispatch_action(s, Action::EditActiveConnection, tx);
                    })),
            ),
        ConnectionState::Connecting | ConnectionState::Reconnecting => col()
            .key("stage-connecting")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(12.0)
            .child(Spinner::new().size(20.0).color(c.text_accent).build(k))
            .child(
                text(format!("Connecting to {}…", session.profile.name))
                    .font(k.ty.body_medium)
                    .color(c.text_muted),
            ),
        ConnectionState::Connected(data) => {
            let Some(active_db) = &data.active_database else {
                return build_no_db_stage(data, k);
            };
            if let Some(active_tab) = session.active_tab() {
                let tab_bar = table_grid::build_tab_bar(session, k, cx);
                let body = match &active_tab.content {
                    TabContent::Table(table_tab) => table_grid::build_table_tab(s, session, table_tab, k, layer),
                };
                return col()
                    .key("stage-tabs-shell")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .child(tab_bar)
                    .child(body);
            }
            col()
                .key("stage-ready")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .center()
                .gap(10.0)
                .padding(32.0)
                .child(icon(&icons::LOGO, 38.0).color(c.icon_faint))
                .child(text(format!("{active_db} ready")).font(k.ty.body_medium).color(c.text_base))
                .child(
                    text("Select a table or view in the sidebar, or press ⌘P to jump directly.")
                        .font(k.ty.ui)
                        .color(c.text_muted),
                )
                .child(
                    row()
                        .gap(8.0)
                        .pt(4.0)
                        .child(empty_cta("Jump to Object (⌘P)", k, true, |s, tx| {
                            s.open_picker(PickerKind::QuickJump, tx);
                        }))
                        .child(empty_cta("Switch Database (⌘K)", k, false, |s, tx| {
                            s.open_picker(PickerKind::Database, tx);
                        })),
                )
        }
    }
}

fn build_no_db_stage(data: &ConnectedData, k: &Kit) -> El {
    let c = &k.c;
    let mut quick_dbs = row().gap(8.0).flex_wrap().center();
    for db in data.databases.iter().take(6) {
        let name = db.name.clone();
        quick_dbs = quick_dbs.child(
            Button::new(&db.name)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .icon(&icons::DATABASE)
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| s.select_database(name.clone(), tx))
                .build(k),
        );
    }

    col()
        .key("stage-no-db")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .gap(12.0)
        .padding(32.0)
        .child(icon(&icons::DATABASE, 38.0).color(c.icon_faint))
        .child(
            text(format!("Connected to {}", data.server_version))
                .font(k.ty.body_medium)
                .color(c.text_base),
        )
        .child(
            text("Choose a database to inspect schemas and tables (⌘K).")
                .font(k.ty.ui)
                .color(c.text_muted),
        )
        .child(empty_cta("Open Database (⌘K)", k, true, |s, tx| {
            s.open_picker(PickerKind::Database, tx);
        }))
        .child(quick_dbs)
}

fn build_console_drawer(session: &SessionState, k: &Kit) -> El {
    let c = &k.c;
    let syntax_theme = SyntaxTheme::from_kit(k);

    let header = row()
        .key("console-header")
        .w_full()
        .h(32.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(icon(&icons::TERMINAL, 13.0).color(c.icon_muted))
        .child(text("Console Log").font(k.ty.small_medium).color(c.text_base))
        .child(
            text(format!("{} queries", session.console_entries.len()))
                .font(k.ty.small)
                .color(c.text_faint),
        )
        .child(spacer())
        .child(
            Button::new("Clear")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .on_press(|s: &mut AppState| {
                    if let Some(sess) = s.active_session_mut() {
                        sess.console_entries.clear();
                    }
                })
                .build(k),
        )
        .child(
            IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .label("Hide console log")
                .on_press(|s: &mut AppState| {
                    if let Some(sess) = s.active_session_mut() {
                        sess.console_open = false;
                    }
                })
                .build(k),
        );

    let body = if session.console_entries.is_empty() {
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .child(
                text("Console log — connect or select a table to inspect executed SQL.")
                    .font(k.ty.ui_compact)
                    .color(c.text_faint),
            )
    } else {
        let mut entries_col = col().key("console-entries").w_full().padding(10.0).gap(8.0);
        for (i, entry) in session.console_entries.iter().enumerate() {
            let meta = format!("-- {} ({} ms)", entry.timestamp, entry.duration_ms);
            let highlights = syntax::highlight(Some("sql"), &entry.sql);
            let spans = syntax_theme.spans(&highlights, 0);

            let mut item = col()
                .key(format!("console/entry/{i}"))
                .w_full()
                .gap(2.0)
                .child(text(meta).font(k.ty.small).color(c.text_faint))
                .child(
                    rich(&entry.sql, spans)
                        .font(k.ty.mono)
                        .color(c.text_base)
                        .w_full()
                        .wrap(),
                );
            if let Some(err) = &entry.error {
                item = item.child(text(err).font(k.ty.small).color(c.danger.fg).w_full().wrap());
            }
            entries_col = entries_col.child(item);
        }
        k.scroll_bar(
            col()
                .key("console-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: true })
                .child(entries_col),
        )
        .w_full()
        .grow(1.0)
        .min_h(0.0)
    };

    col()
        .key("workspace-console")
        .w_full()
        .h(184.0)
        .min_h(0.0)
        .fill(c.bg_base)
        .child(header)
        .child(body)
}
