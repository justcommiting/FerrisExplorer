// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyzer;
mod api;
mod archive;
mod bulk_rename;
mod compare;
mod core;
mod db;
mod desktop;
mod duplicates;
mod extract;
mod git;
mod nl;
mod ops;
mod persistence;
mod plugins;
mod preview;
mod recycle;
mod search;
mod shell;
mod space;
mod state;
mod sys;
#[cfg(test)]
mod testutil;
mod watch;
mod workers;

/// Where panics are recorded. A GUI app launched from the desktop has no
/// visible stderr, so without this a crash leaves no trace.
fn crash_log_path() -> std::path::PathBuf {
    sys::get_home_dir().join(".ferris_explorer_crash.log")
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let thread = std::thread::current();
        let detail = info.to_string();
        let entry = format!(
            "[{when}] thread '{}' panicked: {detail}\n{}\n",
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::capture()
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crash_log_path())
        {
            let _ = f.write_all(entry.as_bytes());
        }
        // A panic inside the logger must not escape the hook: a panic in a
        // panic hook aborts the process. The crash-log write above is the
        // durable record, so a failed log call is simply dropped.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            log::error!("panic: {detail}");
        }));
        default_hook(info);
    }));
}

fn main() {
    install_panic_hook();

    let (state, channels) = state::AppState::new();

    tauri::Builder::default()
        // First, so startup logs from the other plugins and setup are captured.
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                ])
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .max_file_size(5_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state)
        .setup(move |app| {
            state::spawn_pump(app.handle().clone(), channels);
            // Built here rather than in tauri.conf.json so it gets the same
            // per-platform frame as windows opened later.
            api::explorer_window(app.handle(), "main".into()).build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                use tauri::Manager;
                window
                    .state::<state::AppState>()
                    .forget_window(window.label());
            }
        })
        .invoke_handler(tauri::generate_handler![
            api::places,
            api::drive_usage,
            api::list_dir,
            api::list_dir_stream,
            api::cancel_listing,
            api::archive_context,
            api::extract_archive,
            api::resolve_location,
            api::is_navigable,
            api::watch_dirs,
            api::request_dir_sizes,
            api::start_search,
            api::cancel_search,
            api::quick_search,
            api::count_smart_folder,
            api::index_status,
            api::rebuild_index,
            api::space_hoggers,
            api::git_status,
            api::git_diff,
            api::git_stage,
            api::git_unstage,
            api::git_discard,
            api::git_commit,
            api::git_ignored,
            api::git_blame,
            api::preview,
            api::content_matches,
            api::open_path,
            api::reveal_path,
            api::open_terminal,
            api::open_in_editor,
            api::rename_path,
            api::rename_batch,
            api::create_folder,
            api::dir_is_empty,
            api::trash_paths,
            api::delete_permanently,
            api::restore_from_trash,
            api::trash_unchanged_created,
            api::move_exact,
            api::transfer_paths,
            api::resolve_drop,
            api::start_drag,
            api::cancel_op,
            api::resolve_conflict,
            api::space_tree,
            api::space_changes,
            api::cancel_space_scan,
            api::start_duplicate_scan,
            api::cancel_duplicate_scan,
            api::trash_duplicates,
            api::start_compare,
            api::cancel_compare,
            api::compare_bulk,
            api::plan_sync,
            api::run_sync,
            api::cancel_sync,
            api::folder_sizes,
            api::open_window,
            api::drop_tab,
            api::delete_summary,
            api::duplicate_paths,
            api::compress_path,
            api::load_settings,
            api::save_favorites,
            api::save_searches,
            api::push_history,
            api::record_visit,
            api::jump_folders,
            api::load_key_prefs,
            api::save_key_prefs,
            api::plan_shell,
            api::start_shell,
            api::cancel_shell,
            api::load_shell_history,
            api::push_shell_history,
            api::load_workspaces,
            api::load_last_session,
            api::save_workspace,
            api::save_last_session,
            api::delete_workspace,
            api::set_restore_on_launch,
            api::resolve_restore_dirs,
            api::load_plugins,
            api::set_plugin_enabled,
            api::plan_plugin_action,
            api::run_plugin_action,
            api::plugin_preview,
            api::cancel_plugin_preview,
            api::check_for_update,
            api::install_update,
            api::log_frontend_error,
            api::open_log_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running FerrisExplorer");
}
