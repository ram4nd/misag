//! A database that cannot be opened: a damaged one is moved aside and replaced,
//! anything else leaves the files alone and runs on an in-memory database.

use super::schema::{enforce_foreign_keys, migrate};
use super::{unix_secs, Backing, Db, OpenError};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Every path that makes up the database: the file and its WAL sidecars, sidecars first.
pub(super) fn database_files(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", ""].map(|suffix| PathBuf::from(format!("{}{suffix}", path.display())))
}

impl Db {
    fn in_memory(reason: String) -> Self {
        eprintln!("misag: {reason}; tabs will not be saved this run");
        let mut conn = Connection::open_in_memory().expect("in-memory SQLite always opens");
        enforce_foreign_keys(&conn).expect("in-memory SQLite has foreign keys");
        migrate(&mut conn).expect("migrations apply to an empty in-memory database");
        Self::with(Backing::Memory { reason }, conn)
    }

    /// Opens the database without ever failing. A damaged file is moved aside to
    /// `<FILE>.broken-<unix time>` (with its `-wal`/`-shm` files) and replaced. Any
    /// other problem (no data directory, permissions, a lock, a newer schema) leaves
    /// the files untouched and runs on an empty in-memory database instead. Either
    /// way `status()` says what happened.
    pub fn open_or_recover(path: Result<PathBuf, String>) -> Self {
        let path = match path {
            Ok(path) => path,
            Err(e) => return Self::in_memory(format!("there is no app data directory ({e})")),
        };
        let damage = match Self::open(path.clone()) {
            Ok(db) => return db,
            Err(OpenError::Corrupt(e)) => e,
            Err(other) => return Self::in_memory(other.to_string()),
        };
        eprintln!("misag: {damage}; moving the database aside and starting fresh");
        let aside = PathBuf::from(format!("{}.broken-{}", path.display(), unix_secs(SystemTime::now())));
        if let Err(e) = move_files(&database_files(&path), &database_files(&aside)) {
            return Self::in_memory(format!("the database is damaged and could not be moved aside ({e})"));
        }
        let notice = format!(
            "Saved tabs could not be read, so Mischief of Agents started fresh. The old database was kept at {}.",
            aside.display()
        );
        match Self::open_with_notice(path, Some(notice)) {
            Ok(db) => db,
            Err(e) => Self::in_memory(format!("{e}; the damaged database was kept at {}", aside.display())),
        }
    }
}

/// Renames each existing `from` file to the matching `to`, putting back whatever
/// was already moved if one fails. Sidecars come first, so a failure does not leave
/// a stale WAL next to a fresh database (unless putting it back fails too, which
/// is logged).
pub(super) fn move_files(from: &[PathBuf], to: &[PathBuf]) -> std::io::Result<()> {
    let mut moved: Vec<(&PathBuf, &PathBuf)> = Vec::new();
    for (src, dst) in from.iter().zip(to) {
        let result = match src.try_exists() {
            Ok(false) => continue,
            Ok(true) => std::fs::rename(src, dst),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            for (src, dst) in moved.into_iter().rev() {
                if let Err(back) = std::fs::rename(dst, src) {
                    eprintln!("misag: could not move {} back: {back}", dst.display());
                }
            }
            return Err(e);
        }
        moved.push((src, dst));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::{StoreStatus, FILE};
    use crate::layout::Layout;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn moves_a_corrupt_database_and_its_sidecars_aside() {
        let path = temp_db("corrupt");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "this is not a database, just text that is long enough to have a header").unwrap();
        fs::write(dir.join(format!("{FILE}-wal")), "stale wal").unwrap();
        fs::write(dir.join(format!("{FILE}-shm")), "stale shm").unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        db.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", None)] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1);

        let aside = files_named(&dir, &format!("{FILE}.broken-"));
        let main = aside.iter().find(|n| !n.ends_with("-wal") && !n.ends_with("-shm")).unwrap();
        assert!(fs::read_to_string(dir.join(main)).unwrap().starts_with("this is not a database"));
        // SQLite may already have discarded the invalid sidecars; either way the stale
        // ones must not be left next to the new database.
        for sidecar in [format!("{FILE}-wal"), format!("{FILE}-shm")] {
            let left = fs::read_to_string(dir.join(&sidecar)).unwrap_or_default();
            assert!(!left.starts_with("stale"), "{sidecar} still holds the old contents");
        }
        let StoreStatus::Recovered { notice } = db.status() else { panic!("expected Recovered") };
        assert!(notice.contains(main.as_str()));
    }

    #[test]
    fn damage_past_the_header_never_loses_tabs_silently() {
        let path = temp_db("deep-corrupt");
        let db = Db::open(path.clone()).unwrap();
        let tabs = (0..200).map(|i| terminal_tab(&format!("tab-{i:03}-{}", "x".repeat(40)))).collect();
        db.save_layout(Layout { active: 0, tabs }).unwrap();
        drop(db);
        // Fold the WAL back in, then scribble over a page after the header.
        Connection::open(&path).unwrap().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 3 * 4096, "the test needs several pages");
        bytes[2 * 4096..3 * 4096].fill(0xA5);
        fs::write(&path, bytes).unwrap();

        let db = Db::open_or_recover(Ok(path.clone()));
        match db.status() {
            // Detected on open: moved aside and said so.
            StoreStatus::Recovered { .. } => {}
            // Otherwise loading either fails (the frontend then stops saving) or, if
            // the damaged page held no tab rows, still returns every tab.
            _ => {
                if let Ok(layout) = db.load_layout() {
                    assert_eq!(layout.tabs.len(), 200, "a damaged table must not load as fewer tabs");
                }
            }
        }
    }

    #[test]
    fn a_failed_move_puts_everything_back() {
        let dir = temp_db("move-back").parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();
        let from = [dir.join("a-wal"), dir.join("a")];
        fs::write(&from[0], "wal").unwrap();
        fs::write(&from[1], "db").unwrap();
        // The second target's directory does not exist, so that rename fails.
        let to = [dir.join("b-wal"), dir.join("missing").join("b")];
        assert!(move_files(&from, &to).is_err());
        assert_eq!(fs::read_to_string(&from[0]).unwrap(), "wal");
        assert_eq!(fs::read_to_string(&from[1]).unwrap(), "db");
        assert!(!to[0].exists());
    }

    #[test]
    fn leaves_a_newer_database_alone() {
        let path = temp_db("too-new");
        drop(Db::open(path.clone()).unwrap());
        set_version(&path, 99);
        assert!(matches!(Db::open(path.clone()), Err(OpenError::TooNew { found: 99, .. })));
        let db = Db::open_or_recover(Ok(path.clone()));
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.broken-")).is_empty());
        assert_eq!(db.hook_path(), None);
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("newer Mischief of Agents"));
    }

    #[cfg(unix)]
    #[test]
    fn leaves_an_unreadable_database_alone() {
        if crate::test_support::running_as_root() {
            return;
        }
        let path = temp_db("no-access");
        drop(Db::open(path.clone()).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.broken-")).is_empty());
        assert!(path.exists());
        assert!(matches!(db.status(), StoreStatus::NotSaving { .. }));
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1, "still works, in memory");
    }

    #[test]
    fn runs_in_memory_without_a_data_directory() {
        let db = Db::open_or_recover(Err("no home".into()));
        assert_eq!(db.hook_path(), None);
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("no home"), "{reason}");
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_not_used() {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_support::temp_dir("non-utf8");
        let path = dir.join(std::ffi::OsStr::from_bytes(b"misag-\xff.db"));
        assert!(matches!(Db::open(path.clone()), Err(OpenError::Other(_))));
        let db = Db::open_or_recover(Ok(path.clone()));
        assert_eq!(db.hook_path(), None, "the hook could not be told where it is");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_database_that_cannot_be_moved_runs_in_memory() {
        if crate::test_support::running_as_root() {
            return;
        }
        let path = temp_db("corrupt-locked");
        let dir = path.parent().unwrap().to_path_buf();
        fs::write(&path, "this is not a database, just text that is long enough to have a header").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("could not be moved aside"), "{reason}");
        assert!(fs::read_to_string(&path).unwrap().starts_with("this is not a database"), "left alone");
    }
}
