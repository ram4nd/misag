mod pty;
mod session;

use tauri::{Manager, RunEvent};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(session::Sessions::default())
        .invoke_handler(tauri::generate_handler![
            pty::pty_spawn,
            pty::pty_write,
            pty::pty_resize,
            pty::pty_kill,
        ])
        .build(tauri::generate_context!())
        .expect("error while building raterm")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                app.state::<session::Sessions>().kill_all();
            }
        });
}
