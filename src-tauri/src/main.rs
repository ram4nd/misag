// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use misag_lib::{claude, cli, session};
use std::io::Write;

fn main() {
    // `args_os`, so an argument that is not UTF-8 cannot panic before the app starts.
    let Some(mode) = std::env::args_os().nth(1) else {
        return misag_lib::run();
    };
    match mode.to_str() {
        // Claude Code runs this binary as its SessionStart hook for Claude tabs.
        Some(claude::HOOK_FLAG) => std::process::exit(claude::run_hook()),
        // The `claude` wrapper in terminal tabs starts Claude through this binary.
        Some(claude::LAUNCH_FLAG) => std::process::exit(misag_lib::run_launch()),
        // Non-POSIX login shells launch Claude tabs through this binary.
        Some(session::EXEC_FLAG) => std::process::exit(misag_lib::run_exec()),
        // Flows from the command line, for a person or for Claude.
        Some(cli::FLOW_COMMAND) => {
            std::process::exit(cli::run(std::env::args_os().skip(2).collect()))
        }
        // Not print!, which panics on a closed stdout.
        Some("--help" | "-h") => {
            let _ = std::io::stdout().write_all(cli::APP_HELP.as_bytes());
        }
        // Anything else is a mistake, such as a mistyped `flow`: say so rather than
        // start the app, which a person or Claude might not notice.
        _ => {
            let shown = mode.to_string_lossy();
            let _ = writeln!(std::io::stderr(), "misag: unknown command {shown:?}; see misag --help");
            std::process::exit(2)
        }
    }
}
