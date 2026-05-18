pub mod commands;
pub mod io_atomic;
pub mod model;
mod preferences;
pub mod projects;
mod runtime;
pub mod scope;
pub mod watcher;

/// Tauri entry point for the GUI. The caller supplies the build-time
/// `tauri::Context` (via `tauri::generate_context!()` at the bin's compile
/// site) so the macro — which requires the bundled frontend assets — only
/// expands when the GUI bin is being built. The CLI bin re-uses the same
/// library but never calls this function, so it can build without the
/// frontend's `dist/` being present.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run(context: tauri::Context) {
    // Sandbox / scratch-home overrides (#66): parse `--home` / `--project`
    // from argv and `CLAUDE_SCOPE_HOME` / `CLAUDE_SCOPE_PROJECT` from env at
    // launch. Stored as managed state so every subsequent scope::resolve
    // routes through the override when set.
    //
    // `args_os` + `to_string_lossy` instead of `args`: `std::env::args` panics
    // when any argv element is not valid UTF-8. On Unix that's a real risk
    // (filesystem paths can be arbitrary bytes), and crashing before the UI
    // loads is the worst possible failure mode. Lossy conversion replaces
    // invalid bytes with the replacement char; the override paths still
    // resolve sensibly when valid, and the parser silently drops anything it
    // can't classify.
    let overrides = runtime::RuntimeOverrides::from_env_and_args(
        std::env::args_os().map(|a| a.to_string_lossy().into_owned()),
        &runtime::RealEnv,
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Clipboard goes through Tauri's plugin (not `navigator.clipboard`) so
        // Copy / Paste in the right-click menu (#8) hit the OS clipboard via
        // Rust and don't trigger the webview's browser-style permission
        // prompt — desktop UX shouldn't ask "may localhost access your
        // clipboard?" on every right-click.
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(watcher::WatchState::default())
        .manage(overrides)
        .invoke_handler(tauri::generate_handler![
            commands::load_scopes,
            commands::diff_move_leaf,
            commands::apply_move_leaf,
            commands::diff_delete_leaf,
            commands::apply_delete_leaf,
            commands::diff_add_leaf,
            commands::apply_add_leaf,
            commands::load_preferences,
            commands::save_preferences,
            commands::load_runtime_info,
            commands::list_known_projects,
        ])
        .run(context)
        .expect("error while running tauri application");
}
