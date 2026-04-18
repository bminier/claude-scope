mod commands;
mod io_atomic;
mod model;
mod scope;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::load_scopes,
            commands::diff_move,
            commands::apply_move,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
