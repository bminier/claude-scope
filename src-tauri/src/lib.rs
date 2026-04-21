mod commands;
mod io_atomic;
mod model;
mod preferences;
mod scope;
mod watcher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(watcher::WatchState::default())
        .invoke_handler(tauri::generate_handler![
            commands::load_scopes,
            commands::diff_move,
            commands::apply_move,
            commands::diff_move_key,
            commands::apply_move_key,
            commands::load_preferences,
            commands::save_preferences,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
