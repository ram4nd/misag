//! Helpers shared by the integration tests.

use std::path::PathBuf;

/// An empty directory for one test, named after it and this test process.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("raterm-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
