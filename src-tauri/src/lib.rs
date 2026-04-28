mod commands;
mod io_atomic;
mod model;
mod preferences;
mod runtime;
mod scope;
mod watcher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Sandbox / scratch-home overrides (#66): parse `--home` / `--project`
    // from argv and `CLAUDE_SCOPE_HOME` / `CLAUDE_SCOPE_PROJECT` from env at
    // launch. Stored as managed state so every subsequent scope::resolve
    // routes through the override when set.
    let overrides =
        runtime::RuntimeOverrides::from_env_and_args(std::env::args(), &runtime::RealEnv);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(watcher::WatchState::default())
        .manage(overrides)
        .invoke_handler(tauri::generate_handler![
            commands::load_scopes,
            commands::diff_move,
            commands::apply_move,
            commands::diff_move_key,
            commands::apply_move_key,
            commands::load_preferences,
            commands::save_preferences,
            commands::load_runtime_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
