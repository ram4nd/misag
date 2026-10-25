// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use misag_lib::{claude, session};

fn main() {
    // `args_os`, so an argument that is not UTF-8 cannot panic before the app starts.
    let mode = std::env::args_os().nth(1);
    match mode.as_ref().and_then(|a| a.to_str()) {
        // Claude Code runs this binary as its SessionStart hook for Claude tabs.
        Some(claude::HOOK_FLAG) => std::process::exit(claude::run_hook()),
        // The `claude` wrapper in terminal tabs starts Claude through this binary.
        Some(claude::LAUNCH_FLAG) => std::process::exit(misag_lib::run_launch()),
        // Non-POSIX login shells launch Claude tabs through this binary.
        Some(session::EXEC_FLAG) => std::process::exit(misag_lib::run_exec()),
        _ => misag_lib::run(),
    }
}
