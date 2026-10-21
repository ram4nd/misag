use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::ipc::{Channel, Response};
use tauri::State;

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

#[derive(Default)]
pub struct Sessions {
    next_id: AtomicU32,
    map: Mutex<HashMap<u32, Session>>,
}

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
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(&shell);
    if let Some(c) = command.filter(|c| !c.trim().is_empty()) {
        cmd.args(["-lc", &c]);
    } else {
        cmd.arg("-l");
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let dir = cwd
        .filter(|d| !d.is_empty())
        .or_else(|| std::env::var("HOME").ok());
    if let Some(d) = dir {
        cmd.cwd(d);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;

    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if on_data.send(Response::new(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = on_exit.send(0);
    });

    state.map.lock().unwrap().insert(
        id,
        Session { master: pair.master, writer, child },
    );
    Ok(id)
}

#[tauri::command]
pub fn pty_write(state: State<'_, Sessions>, id: u32, data: String) -> Result<(), String> {
    let mut map = state.map.lock().unwrap();
    let s = map.get_mut(&id).ok_or("no such session")?;
    s.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    s.writer.flush().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pty_resize(state: State<'_, Sessions>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.map.lock().unwrap();
    let s = map.get(&id).ok_or("no such session")?;
    s.master
        .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pty_kill(state: State<'_, Sessions>, id: u32) -> Result<(), String> {
    if let Some(mut s) = state.map.lock().unwrap().remove(&id) {
        let _ = s.child.kill();
        let _ = s.child.wait();
    }
    Ok(())
}
