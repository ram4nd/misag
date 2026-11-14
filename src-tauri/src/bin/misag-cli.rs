//! misag's command-line modes as a console program, for Windows: terminals wait for
//! it and show its output, which they do not for the app (a windowed program).
//! Linked into terminal tabs as `misag.exe` and as the `claude.exe` wrapper.

fn main() {
    // Without arguments it says so itself (see `dispatch`).
    std::process::exit(misag_lib::dispatch(misag_lib::Build::Console).unwrap_or(2))
}
