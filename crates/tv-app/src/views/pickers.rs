use rune::{El, Key, ScrollSpec, Tx, col, el, icon, row, spacer, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit, TextInput};
use tv_core::{ConnectionId, DbObjectKind};

use crate::icons;
use crate::state::{AppState, OverlayState, PickerKind, PickerState, Screen};
use crate::theme;
use crate::views::{format_bytes, pill_badge};

enum PickerEntry {
    Database {
        name: String,
        owner: String,
        size: Option<i64>,
        active: bool,
    },
    Connection {
        id: ConnectionId,
        name: String,
        subtitle: String,
        env_tag: &'static str,
        env_pal: theme::EnvPalette,
        open: bool,
    },
    Object {
        schema: String,
        name: String,
        kind: DbObjectKind,
        rows: Option<i64>,
    },
}

/// Builds the modal fuzzy-picker dialog (`Cmd+K`, `Cmd+Shift+K`, or `Cmd+P`).
pub fn build(s: &AppState, picker: &PickerState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let items = collect_entries(s, picker, k.dark);
    let count = items.len();
    let highlighted = if count == 0 {
        None
    } else {
        Some(picker.highlighted.min(count - 1))
    };

    let (title, placeholder, action_label) = match picker.kind {
        PickerKind::Database => ("Open database", "Search for database…", "Open"),
        PickerKind::Connection => ("Switch connection", "Search saved connections…", "Connect"),
        PickerKind::QuickJump => ("Jump to object", "Search tables and views…", "Open"),
    };

    let search = TextInput::new(&picker.query)
        .label(title)
        .placeholder(placeholder)
        .leading_icon(k.glyph.search)
        .clear_button()
        .width(None)
        .on_edit(AppState::picker_query_mut)
        .on_change(|s: &mut AppState, _| {
            if let OverlayState::Picker(p) = &mut s.overlay {
                p.highlighted = 0;
            }
        })
        .on_key(|s: &mut AppState, ev, tx| handle_picker_key(s, ev, tx))
        .on_submit(|s: &mut AppState, tx| activate_highlighted(s, tx))
        .build(k)
        .key("picker-search");

    let mut rows_col = col().key("picker-rows").w_full().gap(2.0);
    if items.is_empty() {
        rows_col = rows_col.child(
            row()
                .w_full()
                .h(48.0)
                .center()
                .child(text("No matching items").font(k.ty.ui).color(c.text_muted)),
        );
    } else {
        for (idx, entry) in items.iter().enumerate() {
            let active = highlighted == Some(idx);
            rows_col = rows_col.child(render_entry(entry, idx, active, k));
        }
    }

    let list = k
        .scroll_bar(
            col()
                .key("picker-scroll")
                .w_full()
                .h((count as f32 * 36.0 + 8.0).clamp(52.0, 320.0))
                .min_h(0.0)
                .padding(4.0)
                .radius(k.radius.md)
                .fill(c.bg_base)
                .border(0.5, c.border_muted)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(rows_col),
        )
        .w_full()
        .min_h(0.0);

    let mut footer = row()
        .key("picker-footer")
        .w_full()
        .pt(4.0)
        .gap(8.0)
        .center_y();

    if picker.kind == PickerKind::Connection {
        footer = footer.child(
            Button::new("Manage Connections…")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                    s.overlay = OverlayState::None;
                    s.screen = Screen::Launcher;
                    s.sync_window_title(tx);
                })
                .build(k),
        );
    }

    footer = footer
        .child(spacer())
        .child(
            Button::new("Cancel")
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Regular)
                .on_press(|s: &mut AppState| s.overlay = OverlayState::None)
                .build(k),
        )
        .child(
            Button::new(action_label)
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Regular)
                .disabled(highlighted.is_none())
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| activate_highlighted(s, tx))
                .build(k),
        );

    let body = col()
        .key("picker-body")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .on_key_preview(|s: &mut AppState, ev, tx| handle_picker_key(s, ev, tx))
        .child(search)
        .child(list)
        .child(footer);

    Dialog::new("picker", title)
        .open(true)
        .width(460.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}

fn collect_entries(s: &AppState, picker: &PickerState, dark: bool) -> Vec<PickerEntry> {
    let q = picker.query.text.trim().to_ascii_lowercase();
    match picker.kind {
        PickerKind::Database => {
            let Some(connected) = s.active_session().and_then(|sess| sess.status.as_connected()) else {
                return Vec::new();
            };
            connected
                .databases
                .iter()
                .filter(|db| q.is_empty() || db.name.to_ascii_lowercase().contains(&q))
                .map(|db| PickerEntry::Database {
                    name: db.name.clone(),
                    owner: db.owner.clone(),
                    size: db.size_bytes,
                    active: connected.active_database.as_deref() == Some(&db.name),
                })
                .collect()
        }
        PickerKind::Connection => s
            .connections
            .iter()
            .filter(|conn| {
                q.is_empty()
                    || conn.name.to_ascii_lowercase().contains(&q)
                    || conn.engine.label().to_ascii_lowercase().contains(&q)
                    || conn.host.to_ascii_lowercase().contains(&q)
                    || conn
                        .database
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
                    || conn
                        .file_path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().to_ascii_lowercase().contains(&q))
            })
            .map(|conn| PickerEntry::Connection {
                id: conn.id,
                name: conn.name.clone(),
                subtitle: conn.endpoint_summary(),
                env_tag: conn.env.short_tag(),
                env_pal: theme::env_colors(conn.env, conn.color, dark),
                open: s.find_session_for_connection(conn.id).is_some(),
            })
            .collect(),
        PickerKind::QuickJump => {
            let Some(connected) = s.active_session().and_then(|sess| sess.status.as_connected()) else {
                return Vec::new();
            };
            connected
                .objects
                .iter()
                .filter(|obj| q.is_empty() || obj.name.to_ascii_lowercase().contains(&q))
                .map(|obj| PickerEntry::Object {
                    schema: obj.schema.clone(),
                    name: obj.name.clone(),
                    kind: obj.kind,
                    rows: obj.estimated_rows,
                })
                .collect()
        }
    }
}

fn render_entry(entry: &PickerEntry, idx: usize, highlighted: bool, k: &Kit) -> El {
    let c = &k.c;
    let base = row()
        .key(format!("picker/item/{idx}"))
        .role("option")
        .focusable(false)
        .w_full()
        .h(34.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(if highlighted { c.layer_03 } else { rune::Color::TRANSPARENT })
        .hover(c.hover)
        .on_press_tx(move |s: &mut AppState, tx: &mut Tx| activate_index(s, idx, tx));

    match entry {
        PickerEntry::Database {
            name,
            owner,
            size,
            active,
        } => {
            let mut r = base
                .label(name)
                .child(icon(&icons::DATABASE, 14.0).color(if *active { c.text_accent } else { c.icon_muted }))
                .child(
                    text(name)
                        .font(if *active { k.ty.ui_medium } else { k.ty.ui_compact })
                        .color(c.text_base)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                );
            if let Some(bytes) = size {
                r = r.child(text(format_bytes(*bytes)).font(k.ty.small).color(c.text_faint));
            } else if !owner.is_empty() {
                r = r.child(text(owner).font(k.ty.small).color(c.text_faint));
            }
            if *active {
                r = r.child(icon(k.glyph.check_small, 14.0).color(c.text_accent));
            }
            r
        }
        PickerEntry::Connection {
            id: _,
            name,
            subtitle,
            env_tag,
            env_pal,
            open,
        } => {
            let mut r = base
                .label(name)
                .child(el().size(8.0, 8.0).radius(4.0).fill(env_pal.dot))
                .child(
                    text(name)
                        .font(k.ty.ui_medium)
                        .color(c.text_base)
                        .shrink()
                        .truncate(),
                )
                .child(pill_badge(
                    format!("picker/env/{idx}"),
                    *env_tag,
                    env_pal.pill_bg,
                    env_pal.pill_fg,
                    env_pal.pill_border,
                    k,
                ))
                .child(
                    text(subtitle)
                        .font(k.ty.small)
                        .color(c.text_faint)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                );
            if *open {
                r = r.child(text("Open").font(k.ty.small_medium).color(c.success.fg));
            }
            r
        }
        PickerEntry::Object {
            schema: _,
            name,
            kind,
            rows,
        } => {
            let obj_icon = match kind {
                DbObjectKind::View => &icons::VIEW_EYE,
                DbObjectKind::MaterializedView => &icons::MATERIALIZED_VIEW,
                _ => &icons::TABLE,
            };
            let mut r = base
                .label(name)
                .child(icon(obj_icon, 14.0).color(c.icon_muted))
                .child(
                    text(name)
                        .font(k.ty.ui_compact)
                        .color(c.text_base)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                );
            if let Some(n) = rows {
                r = r.child(text(format!("~{n} rows")).font(k.ty.small).color(c.text_faint));
            }
            r
        }
    }
}

fn handle_picker_key(s: &mut AppState, ev: &rune::KeyEvent, tx: &mut Tx) -> bool {
    let OverlayState::Picker(ref picker) = s.overlay else {
        return false;
    };
    let count = collect_entries(s, picker, false).len();
    match ev.key {
        Key::Escape => {
            s.overlay = OverlayState::None;
            true
        }
        Key::Down => {
            if let OverlayState::Picker(ref mut p) = s.overlay
                && count > 0
            {
                p.highlighted = (p.highlighted + 1).min(count - 1);
            }
            true
        }
        Key::Up => {
            if let OverlayState::Picker(ref mut p) = s.overlay {
                p.highlighted = p.highlighted.saturating_sub(1);
            }
            true
        }
        Key::Enter => {
            activate_highlighted(s, tx);
            true
        }
        _ => false,
    }
}

fn activate_highlighted(s: &mut AppState, tx: &mut Tx) {
    let idx = match &s.overlay {
        OverlayState::Picker(p) => p.highlighted,
        _ => return,
    };
    activate_index(s, idx, tx);
}

fn activate_index(s: &mut AppState, idx: usize, tx: &mut Tx) {
    let OverlayState::Picker(ref picker) = s.overlay else {
        return;
    };
    let entries = collect_entries(s, picker, false);
    let Some(entry) = entries.into_iter().nth(idx) else {
        return;
    };
    match entry {
        PickerEntry::Database { name, .. } => {
            s.select_database(name, tx);
        }
        PickerEntry::Connection { id, .. } => {
            s.start_or_focus_connection(id, None, None, tx);
        }
        PickerEntry::Object { schema, name, .. } => {
            s.overlay = OverlayState::None;
            s.select_object(schema, name, tx);
        }
    }
}
