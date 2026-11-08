#![cfg(unix)]

pub mod commands;
pub mod events;
pub mod editors;

use sk1ll_daemon::client::Client;
use tauri::{Emitter, Manager};

pub fn run() {
    let client = Client::default_socket().expect("cannot determine the Daemon socket");
    eprintln!("sk1ll-gui: Daemon socket {}", client.socket().display());
    tauri::Builder::default()
        .manage(editors::EditorCache::default())
        .manage(commands::Bridge::new(client.clone()))
        .setup(move |app| {
            let handle = app.handle().clone();
            let task = tauri::async_runtime::spawn(events::forward(client.clone(), move |name, payload| {
                let _ = handle.emit(name, payload);
            }));
            app.manage(EventTask(task));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::skills, commands::skill_tree, commands::skill_file, commands::adopt_unmanaged, commands::fork,
            commands::status, commands::fleet, commands::items, commands::draft, commands::diff, commands::commit_message,
            commands::commit, commands::restore, commands::pull, commands::conflicts, commands::recovered,
            commands::adopt, commands::overwrite, commands::adopt_displaced, commands::discard_displaced,
            commands::harnesses, commands::imports, commands::resolve_import, commands::add_import, commands::update_import, commands::assignment,
            commands::set_assignment, commands::log, commands::undo, commands::join, commands::join_code,
            commands::remove_machine, commands::connection_info, commands::editors, commands::open_in_editor,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run sk1ll-gui");
}

// Abort the one subscription task when the app is dropped.
struct EventTask(tauri::async_runtime::JoinHandle<()>);

impl Drop for EventTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests;
