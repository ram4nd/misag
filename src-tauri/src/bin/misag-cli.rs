//! misag's command-line modes as a console program, for Windows: terminals wait for
//! it and show its output, which they do not for the app (a windowed program).
//! Linked into terminal tabs as `misag.exe` and as the `claude.exe` wrapper.

fn main() {
    let code = misag_lib::dispatch(true).unwrap_or_else(|| {
        eprintln!("misag: give a command; see misag --help");
        2
    });
    std::process::exit(code)
}
