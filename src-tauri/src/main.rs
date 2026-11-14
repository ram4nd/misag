// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    match misag_lib::dispatch(misag_lib::Build::App) {
        Some(code) => std::process::exit(code),
        None => misag_lib::run(),
    }
}
