#![forbid(unsafe_code)]

pub mod icons;
pub mod platform;
pub mod scenes;
pub mod state;
pub mod theme;
pub mod update;
pub mod views;
pub mod worker;

use rune::{App, AppSpec, ColorScheme};

use crate::state::AppState;

/// Launches the TableView desktop or headless Rune runtime.
pub fn run() {
    let spec = AppSpec::<AppState> {
        min_size: Some((860.0, 560.0)),
        scenes: scenes::SCENES,
        setup: Some(|s, waker| {
            if let Some(worker) = &s.worker {
                worker.set_waker(waker.clone());
            }
            if let Some(update) = &s.update_bridge {
                update.set_waker(waker);
            }
        }),
        poll: Some(|s, _, tx| s.apply_worker_events(tx)),
        on_key: Some(platform::handle_global_key),
        ..AppSpec::new("TableView", (1280.0, 800.0), AppState::init_default, views::view)
    };

    let initial_scheme = tv_core::SettingsStore::open_default()
        .and_then(|store| store.load())
        .map(|settings| state::appearance_from_pref(settings.appearance))
        .unwrap_or(ColorScheme::System);

    let base = App::new(spec)
        .color_scheme(initial_scheme)
        .standard_menus()
        .on_quit(|s: &mut AppState, _| s.install_update_on_quit());

    let app = platform::native_menu_items()
        .into_iter()
        .fold(base, App::menu_item);

    app.run();
}
