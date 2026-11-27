use rune::{El, Key, Tx, col, el, row, spacer, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, Dialog, Field, Kit, SegmentedControl, Select, SelectAppearance, Spinner,
    State, TextInput,
};
use tv_core::{AccentColor, ConnectionId, DbEngine, EnvTag, PasswordMode, SafetyPolicy, SslMode};

use crate::state::{AppState, ConnectionFormState, ImportUrlState, OpenFormSelect, OverlayState, PasswordPromptState, TestStatus};
use crate::theme;

/// Builds the modal dialog for creating or editing a database connection.
pub fn build_form(s: &AppState, form: &ConnectionFormState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let is_edit = form.editing_id.is_some();
    let title = format!(
        "{} {} Connection",
        if is_edit { "Edit" } else { "New" },
        form.engine.label()
    );

    let engine_labels: Vec<&str> = DbEngine::ALL.iter().map(|e| e.label()).collect();
    let engine_idx = DbEngine::ALL.iter().position(|e| *e == form.engine).unwrap_or(0);
    let mut engine_seg = SegmentedControl::new(&engine_labels, Some(engine_idx))
        .label("Database engine")
        .width(None);
    if is_edit {
        for i in 0..DbEngine::ALL.len() {
            engine_seg = engine_seg.segment_state(i, State::Disabled);
        }
    } else {
        engine_seg = engine_seg.on_pick(|s: &mut AppState, idx, _| {
            if let Some(engine) = DbEngine::ALL.get(idx).copied() {
                s.set_form_engine(engine);
            }
        });
    }
    let engine_field = Field::new()
        .label("Engine")
        .control(engine_seg.build(k).key("form-engine"))
        .build(k);

    let name_field = Field::new()
        .label("Name")
        .control(
            TextInput::new(&form.name)
                .label("Connection Name")
                .placeholder("greendale_dev")
                .width(None)
                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.name))
                .on_change(|s: &mut AppState, _| {
                    if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                        f.validation_error = None;
                    }
                })
                .build(k)
                .key("form-name"),
        )
        .build(k);

    let mut swatches = row().key("form-swatches").gap(6.0).center_y();
    for color in AccentColor::ALL {
        let selected = form.color == color;
        let fill = theme::swatch_color(color, k.dark);
        swatches = swatches.child(
            el().key(format!("form/swatch/{}", color.label()))
                .role("button")
                .label(format!("Color {}", color.label()))
                .focusable(true)
                .size(22.0, 22.0)
                .radius(6.0)
                .fill(fill)
                .border(if selected { 2.0 } else { 1.0 }, if selected { c.border_focus } else { c.border_base })
                .on_press(move |s: &mut AppState| {
                    if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                        f.color = color;
                    }
                }),
        );
    }

    let env_labels: Vec<&str> = EnvTag::ALL.iter().map(|e| e.label()).collect();
    let env_select = Select::new("form-env", &env_labels)
        .label("Environment")
        .appearance(SelectAppearance::Field)
        .fill()
        .current(Some(form.env.label()))
        .open(form.open_select == OpenFormSelect::Env)
        .on_toggle(|s: &mut AppState| toggle_form_select(s, OpenFormSelect::Env))
        .on_close(|s: &mut AppState| close_form_select(s))
        .on_choose(|s: &mut AppState, val: &str, _| {
            if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                if let Some(env) = EnvTag::from_label(val) {
                    f.env = env;
                    f.color = env.default_color();
                }
                f.open_select = OpenFormSelect::None;
            }
        })
        .build(k, layer);

    let safety_labels: Vec<&str> = SafetyPolicy::ALL.iter().map(|p| p.label()).collect();
    let safety_select = Select::new("form-safety", &safety_labels)
        .label("Safety Mode")
        .appearance(SelectAppearance::Field)
        .fill()
        .current(Some(form.safety.label()))
        .open(form.open_select == OpenFormSelect::Safety)
        .on_toggle(|s: &mut AppState| toggle_form_select(s, OpenFormSelect::Safety))
        .on_close(|s: &mut AppState| close_form_select(s))
        .on_choose(|s: &mut AppState, val: &str, _| {
            if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                if let Some(policy) = SafetyPolicy::from_label(val) {
                    f.safety = policy;
                }
                f.open_select = OpenFormSelect::None;
            }
        })
        .build(k, layer);

    let meta_row = row()
        .w_full()
        .gap(10.0)
        .items_end()
        .child(col().gap(6.0).child(text("Color").font(k.ty.ui_medium).color(c.text_base)).child(swatches))
        .child(
            col().grow(1.0).min_w(0.0).child(
                Field::new()
                    .label("Environment")
                    .control(env_select)
                    .build(k),
            ),
        )
        .child(
            col().grow(1.2).min_w(0.0).child(
                Field::new()
                    .label("Safety Mode")
                    .control(safety_select)
                    .build(k),
            ),
        );

    let top_card = col()
        .key("form-top-card")
        .w_full()
        .padding(12.0)
        .gap(10.0)
        .radius(k.radius.md)
        .fill(c.layer_02)
        .border(0.5, c.border_muted)
        .child(engine_field)
        .child(name_field)
        .child(meta_row);

    let conn_card = if form.engine == DbEngine::Sqlite {
        let sqlite_input = TextInput::new(&form.file_path)
            .label("Database File")
            .placeholder("/path/to/database.db or :memory:")
            .width(None)
            .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.file_path))
            .on_change(|s: &mut AppState, _| {
                if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                    f.validation_error = None;
                }
            })
            .build(k)
            .key("form-sqlite-path");
        let browse_btn = Button::new("Browse…")
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Regular)
            .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.browse_sqlite_file(tx))
            .build(k)
            .key("form-sqlite-browse");

        col()
            .key("form-conn-card")
            .w_full()
            .padding(12.0)
            .gap(10.0)
            .radius(k.radius.md)
            .fill(c.layer_02)
            .border(0.5, c.border_muted)
            .child(
                Field::new()
                    .label("Database File")
                    .suffix("Choose an existing .db / .sqlite / .sqlite3 file, or enter :memory:.")
                    .control(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(el().grow(1.0).min_w(0.0).child(sqlite_input))
                            .child(browse_btn),
                    )
                    .build(k),
            )
    } else {
        let port_placeholder = form.engine.default_port().to_string();
        let user_placeholder = form.engine.default_user();

        let host_port_row = row()
            .w_full()
            .gap(10.0)
            .child(
                col().grow(3.0).min_w(0.0).child(
                    Field::new()
                        .label("Host / Socket")
                        .control(
                            TextInput::new(&form.host)
                                .label("Host or Unix socket")
                                .placeholder("127.0.0.1")
                                .width(None)
                                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.host))
                                .build(k),
                        )
                        .build(k),
                ),
            )
            .child(
                col().w(96.0).child(
                    Field::new()
                        .label("Port")
                        .control(
                            TextInput::new(&form.port)
                                .label("Port")
                                .placeholder(port_placeholder)
                                .width(None)
                                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.port))
                                .build(k),
                        )
                        .build(k),
                ),
            );

        let user_db_row = row()
            .w_full()
            .gap(10.0)
            .child(
                col().grow(1.0).min_w(0.0).child(
                    Field::new()
                        .label("User")
                        .control(
                            TextInput::new(&form.user)
                                .label("User")
                                .placeholder(user_placeholder)
                                .width(None)
                                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.user))
                                .build(k),
                        )
                        .build(k),
                ),
            )
            .child(
                col().grow(1.0).min_w(0.0).child(
                    Field::new()
                        .label("Database")
                        .control(
                            TextInput::new(&form.database)
                                .label("Database")
                                .placeholder("Optional (pick with ⌘K)")
                                .width(None)
                                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.database))
                                .build(k),
                        )
                        .build(k),
                ),
            );

        let pw_disabled = form.password_mode != PasswordMode::Keychain;
        let pw_placeholder = match form.password_mode {
            PasswordMode::Keychain if form.loading_credential => "Loading from keychain…",
            PasswordMode::Keychain => "Stored in system keychain",
            PasswordMode::AskEveryTime => "Prompted on connect",
            PasswordMode::None => "No password",
        };
        let pw_mode_labels: Vec<&str> = PasswordMode::ALL.iter().map(|m| m.label()).collect();
        let pw_mode_select = Select::new("form-pw-mode", &pw_mode_labels)
            .label("Password Mode")
            .appearance(SelectAppearance::Field)
            .fill()
            .current(Some(form.password_mode.label()))
            .open(form.open_select == OpenFormSelect::PasswordMode)
            .on_toggle(|s: &mut AppState| toggle_form_select(s, OpenFormSelect::PasswordMode))
            .on_close(|s: &mut AppState| close_form_select(s))
            .on_choose(|s: &mut AppState, val: &str, _| {
                if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                    if let Some(mode) = PasswordMode::from_label(val) {
                        f.password_mode = mode;
                    }
                    f.open_select = OpenFormSelect::None;
                }
            })
            .build(k, layer);

        let password_row = row()
            .w_full()
            .gap(10.0)
            .items_end()
            .child(
                col().grow(1.3).min_w(0.0).child(
                    Field::new()
                        .label("Password")
                        .control(
                            TextInput::new(&form.password)
                                .label("Password")
                                .placeholder(pw_placeholder)
                                .width(None)
                                .secure()
                                .disabled(pw_disabled)
                                .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.password))
                                .on_change(|s: &mut AppState, _| {
                                    if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                                        f.password_touched = true;
                                    }
                                })
                                .build(k),
                        )
                        .build(k),
                ),
            )
            .child(
                col().grow(1.0).min_w(0.0).child(
                    Field::new()
                        .label("Storage")
                        .control(pw_mode_select)
                        .build(k),
                ),
            );

        let ssl_labels: Vec<&str> = SslMode::ALL.iter().map(|m| m.label()).collect();
        let ssl_select = Select::new("form-ssl-mode", &ssl_labels)
            .label("SSL Mode")
            .appearance(SelectAppearance::Field)
            .fill()
            .current(Some(form.ssl_mode.label()))
            .open(form.open_select == OpenFormSelect::SslMode)
            .on_toggle(|s: &mut AppState| toggle_form_select(s, OpenFormSelect::SslMode))
            .on_close(|s: &mut AppState| close_form_select(s))
            .on_choose(|s: &mut AppState, val: &str, _| {
                if let OverlayState::ConnectionForm(f) = &mut s.overlay {
                    if let Some(mode) = SslMode::from_pg_param(val) {
                        f.ssl_mode = mode;
                    }
                    f.open_select = OpenFormSelect::None;
                }
            })
            .build(k, layer);

        let mut card = col()
            .key("form-conn-card")
            .w_full()
            .padding(12.0)
            .gap(10.0)
            .radius(k.radius.md)
            .fill(c.layer_02)
            .border(0.5, c.border_muted)
            .child(host_port_row)
            .child(user_db_row)
            .child(password_row)
            .child(Field::new().label("SSL Mode").control(ssl_select).build(k));

        if matches!(form.ssl_mode, SslMode::VerifyCa | SslMode::VerifyFull | SslMode::Require) {
            card = card.child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(
                        col().grow(1.0).min_w(0.0).child(
                            Field::new()
                                .label("CA Cert")
                                .control(
                                    TextInput::new(&form.ca_cert)
                                        .label("CA certificate path")
                                        .placeholder("Optional PEM path")
                                        .width(None)
                                        .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.ca_cert))
                                        .build(k),
                                )
                                .build(k),
                        ),
                    )
                    .child(
                        col().grow(1.0).min_w(0.0).child(
                            Field::new()
                                .label("Client Cert")
                                .control(
                                    TextInput::new(&form.cert)
                                        .label("Client certificate path")
                                        .placeholder("Optional PEM path")
                                        .width(None)
                                        .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.cert))
                                        .build(k),
                                )
                                .build(k),
                        ),
                    )
                    .child(
                        col().grow(1.0).min_w(0.0).child(
                            Field::new()
                                .label("Client Key")
                                .control(
                                    TextInput::new(&form.key)
                                        .label("Client private key path")
                                        .placeholder("Optional PEM path")
                                        .width(None)
                                        .on_edit(|s: &mut AppState| s.form_field_mut(|f| &mut f.key))
                                        .build(k),
                                )
                                .build(k),
                        ),
                    ),
            );
        }
        card
    };

    let mut feedback = col().w_full().gap(6.0);
    if let Some(err) = &form.validation_error {
        feedback = feedback.child(
            row()
                .w_full()
                .padding(8.0)
                .radius(k.radius.sm)
                .fill(c.danger.bg)
                .border(0.5, c.danger.border)
                .child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap().w_full()),
        );
    }
    match &form.test_status {
        TestStatus::Idle => {}
        TestStatus::Testing(_) => {
            feedback = feedback.child(
                row()
                    .w_full()
                    .padding(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .fill(c.layer_03)
                    .child(Spinner::new().size(13.0).color(c.icon_muted).build(k))
                    .child(text("Testing connection…").font(k.ty.ui_compact).color(c.text_muted)),
            );
        }
        TestStatus::Ok(msg) => {
            feedback = feedback.child(
                row()
                    .w_full()
                    .padding(8.0)
                    .radius(k.radius.sm)
                    .fill(c.success.bg)
                    .border(0.5, c.success.border)
                    .child(text(msg).font(k.ty.ui_compact).color(c.success.fg).wrap().w_full()),
            );
        }
        TestStatus::Err(msg) => {
            feedback = feedback.child(
                row()
                    .w_full()
                    .padding(8.0)
                    .radius(k.radius.sm)
                    .fill(c.danger.bg)
                    .border(0.5, c.danger.border)
                    .child(text(msg).font(k.ty.ui_compact).color(c.danger.fg).wrap().w_full()),
            );
        }
    }

    let testing = matches!(form.test_status, TestStatus::Testing(_));
    let can_test = !testing && s.worker.is_some();

    let footer = row()
        .key("form-footer")
        .w_full()
        .pt(4.0)
        .gap(8.0)
        .center_y()
        .child(
            Button::new(if testing { "Testing…" } else { "Test" })
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .disabled(!can_test)
                .on_press(|s: &mut AppState| s.test_connection_form())
                .build(k),
        )
        .child(spacer())
        .child(
            Button::new("Cancel")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .on_press(|s: &mut AppState| s.overlay = OverlayState::None)
                .build(k),
        )
        .child(
            Button::new("Save")
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Regular)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.save_connection_form(false, tx))
                .build(k),
        )
        .child(
            Button::new("Connect")
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Regular)
                .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.save_connection_form(true, tx))
                .build(k),
        );

    let body = col()
        .key("form-body")
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .on_key_preview(|s: &mut AppState, ev, tx| {
            if ev.key == Key::Escape {
                s.overlay = OverlayState::None;
                return true;
            }
            if ev.key == Key::Enter && ev.mods.cmd {
                s.save_connection_form(true, tx);
                return true;
            }
            false
        })
        .child(top_card)
        .child(conn_card)
        .child(feedback)
        .child(footer);

    Dialog::new("connection-form", title)
        .open(true)
        .width(560.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}

fn toggle_form_select(s: &mut AppState, which: OpenFormSelect) {
    if let OverlayState::ConnectionForm(f) = &mut s.overlay {
        f.open_select = if f.open_select == which {
            OpenFormSelect::None
        } else {
            which
        };
    }
}

fn close_form_select(s: &mut AppState) {
    if let OverlayState::ConnectionForm(f) = &mut s.overlay {
        f.open_select = OpenFormSelect::None;
    }
}

/// Builds the modal for importing a `postgresql://`, `mysql://`, or `sqlite://` connection URL.
pub fn build_import_url(modal: &ImportUrlState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let input = TextInput::new(&modal.url)
        .label("Connection URL")
        .placeholder("postgresql://..., mysql://..., or sqlite:///path/to/database.db")
        .width(None)
        .on_edit(AppState::import_url_mut)
        .on_submit(|s: &mut AppState, tx| s.submit_import_url(tx))
        .build(k)
        .key("import-url-input");

    let mut body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .child(
            Field::new()
                .label("Connection URL")
                .suffix("Any password in the URL is stored in your OS keychain rather than JSON config.")
                .control(input)
                .build(k),
        );

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
                Button::new("Import")
                    .variant(ButtonVariant::Contrast)
                    .control_size(ControlSize::Regular)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.submit_import_url(tx))
                    .build(k),
            ),
    );

    Dialog::new("import-url", "Import Connection URL")
        .open(true)
        .width(500.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}

/// Builds the modal for prompting a password on `AskEveryTime` connections.
pub fn build_password_prompt(prompt: &PasswordPromptState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let input = TextInput::new(&prompt.password)
        .label("Password")
        .placeholder("Enter database password")
        .width(None)
        .secure()
        .on_edit(AppState::prompt_password_mut)
        .on_submit(|s: &mut AppState, tx| s.submit_password_prompt(tx))
        .build(k)
        .key("password-prompt-input");

    let mut body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .child(
            Field::new()
                .label(format!("Password for {}", prompt.connection_name))
                .suffix("Used for this session only and not saved to disk.")
                .control(input)
                .build(k),
        );

    if let Some(err) = &prompt.error {
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
                Button::new("Connect")
                    .variant(ButtonVariant::Contrast)
                    .control_size(ControlSize::Regular)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.submit_password_prompt(tx))
                    .build(k),
            ),
    );

    Dialog::new("password-prompt", "Authentication Required")
        .open(true)
        .width(420.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}

/// Builds the confirmation dialog before deleting a connection profile.
pub fn build_delete_confirm(s: &AppState, id: ConnectionId, k: &Kit, layer: &mut Vec<El>) {
    let name = s
        .connections
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name.as_str())
        .unwrap_or("this connection");

    let body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(14.0)
        .child(
            text(format!(
                "Delete \"{name}\" and remove any stored password from your keychain?"
            ))
            .font(k.ty.ui)
            .color(k.c.text_muted)
            .wrap()
            .w_full(),
        )
        .child(
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
                    Button::new("Delete")
                        .variant(ButtonVariant::Danger)
                        .control_size(ControlSize::Regular)
                        .on_press(move |s: &mut AppState| s.delete_connection(id))
                        .build(k),
                ),
        );

    Dialog::new("delete-confirm", "Delete Connection")
        .open(true)
        .width(420.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}
