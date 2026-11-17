//! The settings, one row.

use super::{sql, Db};
use crate::settings::{FinishSound, Settings};
use rusqlite::{params, Connection};

impl Db {
    /// The stored settings (the migration created their row with the defaults).
    pub fn load_settings(&self) -> Result<Settings, String> {
        load_settings(&self.conn())
    }

    /// Stores the settings (checked by the caller, see `Settings::validated`),
    /// returning them as stored.
    pub fn save_settings(&self, settings: Settings) -> Result<Settings, String> {
        let changed = self
            .conn()
            .execute(
                "UPDATE settings SET skip_permissions = ?1, workspace = ?2, disable_auto_memory = ?3, font_size = ?4, finish_sound = ?5 WHERE id = 1",
                params![settings.skip_permissions, settings.workspace, settings.disable_auto_memory, settings.font_size, settings.finish_sound],
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err("the settings row is missing, so nothing was saved".into());
        }
        self.load_settings()
    }
}

/// The settings stored in `conn`; see `Db::load_settings`.
pub fn load_settings(conn: &Connection) -> Result<Settings, String> {
    conn.query_row("SELECT skip_permissions, workspace, disable_auto_memory, font_size, finish_sound FROM settings WHERE id = 1", [], |r| {
        // Only a sound: one this version cannot read is Off, rather than losing the other settings.
        let finish_sound = r.get(4).unwrap_or_else(|e| {
            eprintln!("misag: the stored finish sound cannot be read ({e}); using none");
            FinishSound::Off
        });
        Ok(Settings { skip_permissions: r.get(0)?, workspace: r.get(1)?, disable_auto_memory: r.get(2)?, font_size: r.get(3)?, finish_sound })
    })
    .map_err(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::connect_existing;

    #[test]
    fn saving_without_the_settings_row_is_an_error() {
        let path = temp_db("settings-no-row");
        let db = Db::open(path.clone()).unwrap();
        connect_existing(&path).unwrap().execute("DELETE FROM settings", []).unwrap();
        let err = db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap_err();
        assert!(err.contains("settings row is missing"), "{err}");
    }

    #[test]
    fn settings_round_trip_and_start_as_the_defaults() {
        let path = temp_db("settings");
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings::default());
        let all = Settings { skip_permissions: true, workspace: Some("~".into()), disable_auto_memory: true, font_size: 18, finish_sound: FinishSound::Bell };
        let saved = db.save_settings(all.clone()).unwrap();
        assert_eq!(saved, all, "returned as stored");
        drop(db);
        assert_eq!(Db::open(path).unwrap().load_settings().unwrap(), saved);
    }

    #[test]
    fn the_schema_refuses_settings_values_it_does_not_know() {
        let path = temp_db("settings-check");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        assert!(conn.execute("UPDATE settings SET skip_permissions = 2", []).is_err());
        assert!(conn.execute("UPDATE settings SET disable_auto_memory = 2", []).is_err());
        assert!(conn.execute("UPDATE settings SET font_size = 7", []).is_err());
        assert!(conn.execute("UPDATE settings SET font_size = 33", []).is_err());
        assert!(conn.execute("UPDATE settings SET finish_sound = 'siren'", []).is_err());
        assert!(conn.execute("INSERT INTO settings (id) VALUES (2)", []).is_err(), "one row only");
        assert_eq!(db.load_settings().unwrap(), Settings::default());
    }

    #[test]
    fn every_finish_sound_round_trips() {
        let db = Db::open(temp_db("finish-sounds")).unwrap();
        for sound in [FinishSound::Chime, FinishSound::Bell, FinishSound::Pop, FinishSound::Off] {
            let saved = db.save_settings(Settings { finish_sound: sound, ..Settings::default() }).unwrap();
            assert_eq!(saved.finish_sound, sound);
        }
    }

    #[test]
    fn an_unknown_stored_finish_sound_reads_as_off_and_keeps_the_other_settings() {
        let path = temp_db("finish-sound-unknown");
        let db = Db::open(path.clone()).unwrap();
        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        let conn = connect_existing(&path).unwrap();
        conn.pragma_update(None, "ignore_check_constraints", true).unwrap();
        conn.execute("UPDATE settings SET finish_sound = 'siren'", []).unwrap();
        let loaded = db.load_settings().unwrap();
        assert_eq!(loaded, Settings { skip_permissions: true, ..Settings::default() });
        // So the Settings tab can still save, which replaces it.
        assert_eq!(db.save_settings(Settings { finish_sound: FinishSound::Pop, ..loaded }).unwrap().finish_sound, FinishSound::Pop);
    }
}
