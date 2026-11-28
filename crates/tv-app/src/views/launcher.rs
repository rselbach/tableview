use rune::{Cx, El, Key, ScrollSpec, Tx, col, el, icon, row, spacer, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, Divider, IconButton, IconButtonVariant, Kit, MenuItem, Select,
    SelectAppearance, TextInput, context_menu,
};
use tv_core::{ConnectionProfile, DbEngine, EnvTag, SafetyPolicy, SortOrder};

use crate::icons;
use crate::platform::{self, Action};
use crate::state::{AppState, Screen};
use crate::theme;
use crate::views::{empty_cta, pill_badge, update_card};

/// Builds the integrated Connection Launcher screen.
pub fn build(s: &AppState, k: &Kit, cx: &Cx<AppState>, layer: &mut Vec<El>) -> El {
    let c = &k.c;
    let top_inset = if cx.titlebar_inset() > 0.0 { 38.0 } else { 14.0 };

    let rail = build_left_rail(s, k, top_inset);
    let main = build_main_panel(s, k, layer);

    row()
        .key("launcher-screen")
        .w_full()
        .h_full()
        .min_h(0.0)
        .fill(c.bg_deep)
        .child(rail)
        .child(main)
}

fn build_left_rail(s: &AppState, k: &Kit, top_inset: f32) -> El {
    let c = &k.c;
    let header_drag = el().key("launcher-rail-drag").w_full().h(top_inset).window_drag();

    let brand = row()
        .key("launcher-brand")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            el().key("launcher-logo-box")
                .size(36.0, 36.0)
                .radius(k.radius.md)
                .fill(c.layer_02)
                .border(0.5, c.border_base)
                .center()
                .child(icon(&icons::LOGO, 20.0).color(c.text_accent)),
        )
        .child(
            col().key("launcher-title-col")
                .gap(1.0)
                .child(text("TableView").font(k.ty.body_medium).color(c.text_base))
                .child(
                    text(format!("v{} · SQL Client", s.update.running_version))
                        .font(k.ty.small)
                        .color(c.text_muted),
                ),
        );

    let actions = col()
        .key("launcher-rail-actions")
        .w_full()
        .gap(6.0)
        .child(
            row().w_full().child(
                Button::new("New Connection")
                    .variant(ButtonVariant::Contrast)
                    .control_size(ControlSize::Large)
                    .icon(&icons::PLUS)
                    .shortcut("mod+n")
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_new_connection_form(tx))
                    .build(k)
                    .w_full(),
            ),
        )
        .child(
            row().w_full().child(
                Button::new("Import URL…")
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .icon(&icons::LINK_IMPORT)
                    .shortcut("mod+i")
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_import_url_dialog(tx))
                    .build(k)
                    .w_full(),
            ),
        );

    let mut filters = col().key("launcher-env-filters").w_full().gap(2.0).child(
        text("ENVIRONMENTS")
            .font(k.ty.small_medium)
            .color(c.text_faint)
            .px(8.0)
            .pb(4.0),
    );
    filters = filters.child(env_filter_row("All Connections", None, s.connections.len(), s.env_filter.is_none(), k));
    for env in EnvTag::ALL {
        let count = s.connections.iter().filter(|conn| conn.env == env).count();
        let active = s.env_filter == Some(env);
        filters = filters.child(env_filter_row(env.label(), Some(env), count, active, k));
    }

    let mut sessions_section = col().key("launcher-open-sessions").w_full().gap(4.0);
    if !s.sessions.is_empty() {
        sessions_section = sessions_section
            .child(
                text("OPEN SESSIONS")
                    .font(k.ty.small_medium)
                    .color(c.text_faint)
                    .px(8.0)
                    .pt(6.0),
            );
        let mut sessions: Vec<_> = s.sessions.values().collect();
        sessions.sort_by_key(|sess| sess.id);
        for sess in sessions {
            let sid = sess.id;
            let env_pal = theme::env_colors(sess.profile.env, sess.profile.color, k.dark);
            sessions_section = sessions_section.child(
                row()
                    .key(format!("launcher/session/{}", sid.get()))
                    .w_full()
                    .h(28.0)
                    .px(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .hover(c.hover)
                    .press(c.pressed)
                    .focusable(true)
                    .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                        s.screen = Screen::Workspace(sid);
                        s.sync_window_title(tx);
                    })
                    .child(el().size(7.0, 7.0).radius(3.5).fill(env_pal.dot))
                    .child(
                        text(&sess.profile.name)
                            .font(k.ty.ui_compact)
                            .color(c.text_base)
                            .grow(1.0)
                            .shrink()
                            .truncate(),
                    ),
            );
        }
    }

    let mut rail_inner = col()
        .key("launcher-rail-inner")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pb(16.0)
        .gap(16.0)
        .child(brand)
        .child(actions)
        .child(Divider::horizontal().build(k))
        .child(filters)
        .child(sessions_section)
        .child(spacer());

    if let Some(card) = update_card::build(s, k) {
        rail_inner = rail_inner.child(card);
    }

    col()
        .key("launcher-rail")
        .w(248.0)
        .h_full()
        .min_h(0.0)
        .fill(c.bg_base)
        .border_sides([0.0, 1.0, 0.0, 0.0], c.border_muted)
        .child(header_drag)
        .child(rail_inner)
}

fn env_filter_row(label: &str, env: Option<EnvTag>, count: usize, active: bool, k: &Kit) -> El {
    let c = &k.c;
    let dot = env.map(|e| theme::env_colors(e, e.default_color(), k.dark).dot);
    row()
        .key(format!("launcher/filter/{label}"))
        .role("button")
        .label(label)
        .focusable(true)
        .w_full()
        .h(28.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(if active { c.layer_02 } else { rune::Color::TRANSPARENT })
        .hover(c.hover)
        .press(c.pressed)
        .on_press(move |s: &mut AppState| s.env_filter = env)
        .child(match dot {
            Some(color) => el().size(7.0, 7.0).radius(3.5).fill(color),
            None => icon(&icons::PLUG, 13.0).color(c.icon_muted),
        })
        .child(
            text(label)
                .font(if active { k.ty.ui_medium } else { k.ty.ui })
                .color(if active { c.text_base } else { c.text_muted })
                .grow(1.0)
                .shrink()
                .truncate(),
        )
        .child(text(count.to_string()).font(k.ty.small).color(c.text_faint))
}

fn build_main_panel(s: &AppState, k: &Kit, layer: &mut Vec<El>) -> El {
    let c = &k.c;
    let sort_options = ["Name", "Environment", "Recent"];
    let sort_select = Select::new("launcher-sort", &sort_options)
        .label("Sort connections")
        .appearance(SelectAppearance::Field)
        .width(126.0)
        .current(Some(s.sort_order.label()))
        .open(s.sort_dropdown_open)
        .on_toggle(|s: &mut AppState| s.sort_dropdown_open = !s.sort_dropdown_open)
        .on_close(|s: &mut AppState| s.sort_dropdown_open = false)
        .on_choose(|s: &mut AppState, val: &str, _| {
            if let Some(order) = SortOrder::from_label(val) {
                s.sort_connections(order);
            }
            s.sort_dropdown_open = false;
        })
        .build(k, layer);

    let search = TextInput::new(&s.launcher_search)
        .label("Search connections")
        .placeholder("Search connections by name, host, user, or database…")
        .leading_icon(k.glyph.search)
        .clear_button()
        .width(None)
        .on_edit(|s: &mut AppState| &mut s.launcher_search)
        .on_key(|s: &mut AppState, ev, tx| {
            if ev.key == Key::Enter
                && let Some(first) = s.filtered_connections().first().map(|c| c.id)
            {
                s.start_or_focus_connection(first, None, None, tx);
                return true;
            }
            false
        })
        .build(k);

    let top_bar = row()
        .key("launcher-topbar")
        .w_full()
        .h(52.0)
        .px(18.0)
        .gap(10.0)
        .center_y()
        .window_drag()
        .fill(c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(el().key("launcher-search-wrap").grow(1.0).min_w(0.0).child(search))
        .child(sort_select)
        .child(
            row()
                .gap(4.0)
                .center_y()
                .child(
                    IconButton::new(&icons::PLUS)
                        .variant(IconButtonVariant::Ghost)
                        .control_size(ControlSize::Regular)
                        .label("New connection")
                        .help("New connection")
                        .shortcut("mod+n")
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_new_connection_form(tx))
                        .build(k),
                )
                .child(
                    IconButton::new(&icons::SETTINGS)
                        .variant(IconButtonVariant::GhostMuted)
                        .control_size(ControlSize::Regular)
                        .label("Settings")
                        .help("Settings")
                        .shortcut("mod+comma")
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                            platform::dispatch_action(s, Action::OpenSettings, tx);
                        })
                        .build(k)
                        .key("launcher-settings-btn"),
                ),
        );

    let filtered = s.filtered_connections();
    let list_body = if filtered.is_empty() {
        build_empty_state(s, k)
    } else {
        let mut rows = col().key("launcher-rows").w_full().padding(16.0).gap(6.0);
        for profile in filtered {
            rows = rows.child(build_connection_row(s, profile, k));
        }
        k.scroll_bar(
            col()
                .key("launcher-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(rows),
        )
        .w_full()
        .grow(1.0)
        .min_h(0.0)
    };

    let ctx_target_id = s.launcher_context_menu.map(|(id, _)| id);
    let ctx_point = s.launcher_context_menu.map(|(_, pt)| pt);
    let menu_wrapped_body = if let Some(id) = ctx_target_id {
        context_menu("launcher-ctx", list_body, ctx_point)
            .item(
                MenuItem::new("Connect")
                    .icon(&icons::PLUG)
                    .on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
                        s.start_or_focus_connection(id, None, None, tx);
                    }),
            )
            .item(MenuItem::new("Edit…").on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
                s.open_edit_connection_form(id, tx);
            }))
            .item(MenuItem::new("Duplicate").on_select(move |s: &mut AppState| {
                s.duplicate_connection(id);
            }))
            .item(
                MenuItem::new("Copy as URL")
                    .icon(k.glyph.copy)
                    .on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
                        s.copy_connection_url(id, tx);
                    }),
            )
            .separator()
            .item(MenuItem::new("Delete").danger(true).on_select(move |s: &mut AppState| {
                s.open_delete_confirm(id);
            }))
            .on_close(|s: &mut AppState| s.launcher_context_menu = None)
            .build(k, layer)
    } else {
        list_body
    };

    let mut main_col = col()
        .key("launcher-main")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .fill(c.bg_deep)
        .child(top_bar);
    if let Some(ref err) = s.store_load_error {
        main_col = main_col.child(build_store_load_banner(s, err, k));
    }
    main_col.child(menu_wrapped_body)
}

fn build_store_load_banner(s: &AppState, err: &str, k: &Kit) -> El {
    let c = &k.c;
    let mut actions = row().gap(8.0).center_y().child(
        Button::new("Retry Load")
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut AppState| s.retry_load_connections())
            .build(k)
            .key("launcher-store-retry"),
    );
    if s.persist_enabled {
        actions = actions.child(
            Button::new("Back Up & Start Fresh")
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut AppState| s.backup_and_reset_connection_store())
                .build(k)
                .key("launcher-store-reset"),
        );
    }

    row()
        .key("launcher-store-error")
        .w_full()
        .px(18.0)
        .py(10.0)
        .gap(12.0)
        .center_y()
        .fill(c.danger.bg)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.danger.border)
        .child(
            el()
                .grow(1.0)
                .min_w(0.0)
                .child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap().w_full()),
        )
        .child(actions)
}

fn build_empty_state(s: &AppState, k: &Kit) -> El {
    let c = &k.c;
    if s.store_load_error.is_some() {
        return col()
            .key("launcher-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .padding(32.0)
            .child(icon(&icons::DATABASE, 36.0).color(c.icon_faint))
            .child(
                text("Unable to load saved connections")
                    .font(k.ty.body_medium)
                    .color(c.text_base),
            )
            .child(
                text("Your connections file was preserved on disk. Fix or reload it, or back it up to start fresh.")
                    .font(k.ty.ui)
                    .color(c.text_muted),
            );
    }
    let is_filtered = !s.launcher_search.text.trim().is_empty() || s.env_filter.is_some();
    let title = if is_filtered {
        "No connections match this filter"
    } else {
        "No connections saved yet"
    };
    let subtitle = if is_filtered {
        "Clear your search or switch environment filter to view saved profiles."
    } else {
        "Create a connection profile or paste a postgresql://, mysql://, or sqlite:// URL to get started."
    };

    col()
        .key("launcher-empty")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .center()
        .gap(12.0)
        .padding(32.0)
        .child(icon(&icons::DATABASE, 36.0).color(c.icon_faint))
        .child(text(title).font(k.ty.body_medium).color(c.text_base))
        .child(text(subtitle).font(k.ty.ui).color(c.text_muted))
        .child(
            row()
                .gap(8.0)
                .pt(4.0)
                .child(empty_cta("New Connection", k, true, |s, tx| {
                    s.open_new_connection_form(tx)
                }))
                .child(empty_cta("Import URL…", k, false, |s, tx| {
                    s.open_import_url_dialog(tx)
                })),
        )
}

fn build_connection_row(s: &AppState, profile: &ConnectionProfile, k: &Kit) -> El {
    let c = &k.c;
    let id = profile.id;
    let selected = s.launcher_selected == Some(id);
    let active_sid = s.find_session_for_connection(id);
    let env_pal = theme::env_colors(profile.env, profile.color, k.dark);

    let engine_emblem = el()
        .key(format!("launcher/conn/{id}/emblem"))
        .label(profile.engine.label())
        .size(34.0, 34.0)
        .radius(17.0)
        .fill(env_pal.pill_bg)
        .border(1.0, env_pal.pill_border)
        .center()
        .child(icon(icons::engine(profile.engine), 20.0).color(env_pal.pill_fg));

    let mut top_line = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            text(&profile.name)
                .font(k.ty.ui_medium)
                .color(c.text_base)
                .shrink()
                .truncate(),
        )
        .child(pill_badge(
            format!("launcher/conn/{id}/env"),
            profile.env.short_tag(),
            env_pal.pill_bg,
            env_pal.pill_fg,
            env_pal.pill_border,
            k,
        ));

    if profile.safety != SafetyPolicy::Unrestricted {
        top_line = top_line.child(pill_badge(
            format!("launcher/conn/{id}/safety"),
            profile.safety.label(),
            c.layer_03,
            c.text_muted,
            c.border_muted,
            k,
        ));
    }

    if active_sid.is_some() {
        top_line = top_line.child(
            row()
                .gap(4.0)
                .center_y()
                .child(el().size(6.0, 6.0).radius(3.0).fill(c.success.icon))
                .child(text("Open").font(k.ty.small_medium).color(c.success.fg)),
        );
    }

    let subtitle = if profile.engine == DbEngine::Sqlite {
        profile.endpoint_summary()
    } else {
        format!("{} · {}", profile.endpoint_summary(), profile.user)
    };

    let info_col = col()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(2.0)
        .child(top_line)
        .child(text(subtitle).font(k.ty.caption()).color(c.text_muted).truncate());

    let connect_label = if active_sid.is_some() { "Resume" } else { "Connect" };
    let row_actions = row()
        .gap(6.0)
        .center_y()
        .child(
            Button::new("Edit")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| s.open_edit_connection_form(id, tx))
                .build(k),
        )
        .child(
            Button::new(connect_label)
                .variant(if active_sid.is_some() {
                    ButtonVariant::Contrast
                } else {
                    ButtonVariant::Outline
                })
                .control_size(ControlSize::Small)
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    s.start_or_focus_connection(id, None, None, tx);
                })
                .build(k),
        );

    row()
        .key(format!("launcher/conn/{id}"))
        .role("listitem")
        .label(&profile.name)
        .focusable(true)
        .w_full()
        .h(60.0)
        .px(14.0)
        .gap(12.0)
        .center_y()
        .radius(k.radius.md)
        .fill(if selected { c.layer_02 } else { c.bg_base })
        .border(1.0, if selected { c.border_strong } else { c.border_muted })
        .hover(c.hover)
        .press(c.pressed)
        .on_press(move |s: &mut AppState| s.launcher_selected = Some(id))
        .on_double_press_tx(move |s: &mut AppState, tx: &mut Tx| {
            s.start_or_focus_connection(id, None, None, tx);
        })
        .on_secondary_press(move |s: &mut AppState, press, _| {
            s.launcher_selected = Some(id);
            s.launcher_context_menu = Some((id, press));
        })
        .child(engine_emblem)
        .child(info_col)
        .child(row_actions)
}
