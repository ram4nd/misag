//! Tauri commands over `session::Sessions`.

use crate::session::{Sessions, SpawnRequest};
use tauri::ipc::{Channel, Response};
use tauri::State;

#[tauri::command]
pub fn pty_spawn(
    state: State<'_, Sessions>,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    command: Option<String>,
    on_data: Channel<Response>,
    on_exit: Channel<i32>,
) -> Result<u32, String> {
    state.spawn(
        SpawnRequest { cols, rows, cwd, command },
        move |bytes| on_data.send(Response::new(bytes.to_vec())).is_ok(),
        move |code| {
            if on_exit.send(code).is_err() {
                eprintln!("raterm: exit notification not delivered");
            }
        },
    )
}

#[tauri::command]
pub fn pty_write(state: State<'_, Sessions>, id: u32, data: Vec<u8>) -> Result<(), String> {
    state.write(id, data)
}

#[tauri::command]
pub fn pty_resize(state: State<'_, Sessions>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    state.resize(id, cols, rows)
}

#[tauri::command]
pub fn pty_kill(state: State<'_, Sessions>, id: u32) {
    state.kill(id);
}
