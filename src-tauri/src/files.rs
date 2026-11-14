//! Directory listings for the sidebar's file tree. Mirrored by `Listing` in src/main.ts.

use serde::Serialize;

/// At most this many entries are returned per directory (after sorting), so a huge
/// one cannot flood the tree. The directory is still read in full.
pub const MAX_ENTRIES: usize = 2000;

#[derive(Serialize, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

/// One directory level, folders first, each group sorted by name (byte order).
#[derive(Serialize, Debug, PartialEq)]
pub struct Listing {
    /// The directory listed, with a leading `~` expanded, so children can be joined to it.
    pub dir: String,
    pub entries: Vec<Entry>,
    /// More than `MAX_ENTRIES` entries were found; the rest are left out.
    pub truncated: bool,
    /// Entries left out: hidden ones, names that are not UTF-8, and ones that could not be read.
    pub skipped: usize,
}

/// Lists `dir`, leaving out hidden entries (a leading `.`), names that are not UTF-8
/// (they could not be typed as a path) and entries that cannot be read. A symlink to
/// a directory counts as one; one whose target cannot be reached counts as a file.
pub fn list(dir: &str) -> Result<Listing, String> {
    let dir = crate::session::expand_tilde(dir);
    let read = std::fs::read_dir(&dir).map_err(|e| format!("cannot read {dir}: {e}"))?;
    let (mut entries, mut skipped) = (Vec::new(), 0);
    for entry in read {
        // One unreadable entry (say, deleted while listing) should not hide the rest.
        let Ok(entry) = entry else {
            skipped += 1;
            continue;
        };
        let name = match entry.file_name().into_string() {
            Ok(name) if !name.starts_with('.') => name,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let is_dir = match entry.file_type() {
            Ok(t) if !t.is_symlink() => t.is_dir(),
            // A symlink, or a type the directory entry does not record: ask the target.
            _ => std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
        };
        entries.push(Entry { name, dir: is_dir });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok(Listing { dir, entries, truncated, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn names(listing: &Listing) -> Vec<(&str, bool)> {
        listing.entries.iter().map(|e| (e.name.as_str(), e.dir)).collect()
    }

    #[cfg(unix)]
    #[test]
    fn lists_folders_first_without_hidden_entries() {
        let dir = crate::test_support::temp_dir("files-list");
        for d in ["src", "Docs", ".git"] {
            fs::create_dir(dir.join(d)).unwrap();
        }
        for f in ["b.txt", "a.txt", ".env"] {
            fs::write(dir.join(f), "").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("src"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        let listing = list(dir.to_str().unwrap()).unwrap();
        assert_eq!(
            names(&listing),
            [("Docs", true), ("link", true), ("src", true), ("a.txt", false), ("b.txt", false), ("dangling", false)]
        );
        assert!(!listing.truncated);
        assert_eq!(listing.skipped, 2, ".git and .env");
    }

    #[test]
    fn huge_directories_are_cut_short() {
        let dir = crate::test_support::temp_dir("files-huge");
        for i in 0..=MAX_ENTRIES {
            fs::write(dir.join(format!("f{i:05}")), "").unwrap();
        }
        let listing = list(dir.to_str().unwrap()).unwrap();
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert!(listing.truncated);
    }

    #[test]
    fn expands_the_home_directory_and_reports_errors() {
        let home = crate::session::home_dir().unwrap();
        assert_eq!(list("~").unwrap().dir, home);
        let err = list("/no/such/dir").unwrap_err();
        assert!(err.contains("/no/such/dir"), "{err}");
    }
}
