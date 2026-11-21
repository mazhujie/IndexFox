//! Tauri host for desktop search, background indexing and settings.

mod commands;
mod desktop;
mod scheduler;
mod state;
mod storage;

use indexfox_core::AppConfig;
use state::AppState;
use tauri::{Listener, Manager, RunEvent, WindowEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = AppConfig::default();
    let desktop = desktop::DesktopState::load(config.data_dir.join("config").join("desktop.json"));
    let app = tauri::Builder::default()
        .manage(AppState::new(config))
        .manage(desktop)
        .setup(|app| {
            desktop::setup(app)?;
            let handle = app.handle().clone();
            app.listen(desktop::EXIT_REQUESTED_EVENT, move |_| handle.exit(0));
            app.state::<AppState>().scheduler.start();
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if desktop::close_to_tray(window) {
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::search_documents,
            commands::cancel_search,
            commands::list_sources,
            commands::pick_index_folder,
            commands::index_folder,
            commands::remove_source,
            commands::preview_text,
            commands::render_pdf_page,
            commands::history_settings,
            commands::set_history_enabled,
            commands::clear_history,
            commands::get_index_runtime,
            commands::set_index_paused,
            commands::set_resource_mode,
            commands::get_ocr_settings,
            commands::set_ocr_settings,
            commands::cancel_index_job,
            commands::get_source_config,
            commands::set_source_config,
            commands::scan_source,
            commands::rebuild_indexes,
            commands::get_database_path,
            commands::migrate_database,
            commands::clear_index_cache,
            desktop::get_desktop_settings,
            desktop::set_desktop_settings,
            desktop::system_fonts,
        ])
        .build(tauri::generate_context!())
        .expect("error while running IndexFox");
    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            let state = handle.state::<AppState>();
            state.scheduler.shutdown();
            if let Ok(location) = state.database.try_read() {
                if let Err(error) = storage::checkpoint(location.path()) {
                    eprintln!("indexfox: {error}");
                }
            };
        }
    });
}
