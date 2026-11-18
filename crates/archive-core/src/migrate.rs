//! Turning a library's password on and off, and unlocking it.
//!
//! A library is plain, or sealed under a library key. Sealing it converts
//! the catalogue database and every file under `store/` and `thumbs/`; the
//! key is wrapped under the password in `library.key`, which also records how
//! far a conversion has got. Every step is safe to run again, so an interrupted
//! conversion finishes the next time the library is unlocked.
//!
//! `vault/` is left alone in both directions: its files are already sealed
//! under each locked collection's own key. `tmp/` and `logs/` are not library
//! content and are not converted.
//!
//! Conversions hold the library's file gate for their whole run, so no reader
//! sees a half-converted file. The database is converted by exporting it to a
//! new file under the other key and swapping the file in; SQLCipher cannot
//! encrypt a plain database in place.

use crate::applock::{self, KeyFile, KeyState};
use crate::crypto::Key;
use crate::db;
use crate::error::{Error, Result};
use crate::sealed;
use crate::{Library, write_gate};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

/// How far a conversion has got: `done` of `total` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

/// What the library's password state is right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encryption {
    /// No password. Everything is open.
    Plain,
    /// A password is set and the library has not been unlocked yet.
    Locked,
    /// Sealed, and unlocked in this session.
    Open,
    /// A conversion is part way through, and this session is unlocked.
    Converting,
}

impl Library {
    /// The library's password state. See [`Encryption`].
    pub fn encryption(&self) -> Encryption {
        match applock::read(self.root()) {
            Ok(None) => Encryption::Plain,
            Ok(Some(file)) => {
                if !self.paths().key.is_set() {
                    Encryption::Locked
                } else if file.state == KeyState::Sealed {
                    Encryption::Open
                } else {
                    Encryption::Converting
                }
            }
            Err(_) => Encryption::Locked,
        }
    }

    /// Unlock a sealed library with its password, and finish any conversion
    /// that was interrupted. `Ok(false)` means the password is wrong. A plain
    /// library is always unlocked.
    pub fn unlock_library(&self, password: &str) -> Result<bool> {
        if self.paths().key.is_set() {
            return Ok(true);
        }
        let Some(file) = applock::read(self.root())? else {
            return Ok(true);
        };
        let Some(key) = applock::unlock(&file.lock, password)? else {
            return Ok(false);
        };
        self.attach(key, file.state, &mut |_| {})?;
        Ok(true)
    }

    /// Give a plain library a password, sealing everything under a new key.
    pub fn set_password(&self, password: &str, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let _files = write_gate(&self.files);
        if applock::read(self.root())?.is_some() || self.paths().key.is_set() {
            return Err(Error::msg("this library already has a password"));
        }
        let (lock, key) = applock::create(password)?;
        applock::write(
            self.root(),
            &KeyFile {
                state: KeyState::Encrypting,
                lock,
            },
        )?;
        self.paths().key.set(Some(key));
        self.finish_encrypt(progress)
    }

    /// Replace the password. The library key is rewrapped, so nothing is
    /// re-encrypted. `Ok(false)` means the current password is wrong.
    pub fn change_password(&self, current: &str, new: &str) -> Result<bool> {
        let Some(file) = applock::read(self.root())? else {
            return Err(Error::msg("this library has no password"));
        };
        if file.state != KeyState::Sealed {
            return Err(Error::msg("finish unlocking the library first"));
        }
        let Some(key) = applock::unlock(&file.lock, current)? else {
            return Ok(false);
        };
        let lock = applock::wrap(&key, new)?;
        applock::write(
            self.root(),
            &KeyFile {
                state: KeyState::Sealed,
                lock,
            },
        )?;
        Ok(true)
    }

    /// Remove the password, converting the library back to plain. `Ok(false)`
    /// means the password is wrong.
    pub fn remove_password(
        &self,
        current: &str,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<bool> {
        let _files = write_gate(&self.files);
        let Some(file) = applock::read(self.root())? else {
            return Err(Error::msg("this library has no password"));
        };
        if file.state != KeyState::Sealed {
            return Err(Error::msg("finish unlocking the library first"));
        }
        if applock::unlock(&file.lock, current)?.is_none() {
            return Ok(false);
        }
        if !self.paths().key.is_set() {
            return Err(Error::msg(
                "unlock the library before removing its password",
            ));
        }
        applock::write(
            self.root(),
            &KeyFile {
                state: KeyState::Decrypting,
                lock: file.lock,
            },
        )?;
        self.finish_decrypt(progress)?;
        Ok(true)
    }

    /// Load `key` as this library's key and bring the catalogue up. A sealed
    /// library just opens. An unfinished conversion is finished first.
    fn attach(&self, key: Key, state: KeyState, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        {
            let _files = write_gate(&self.files);
            self.paths().key.set(Some(key));
            let result = self.open_catalogue().and_then(|()| match state {
                KeyState::Sealed => Ok(()),
                KeyState::Encrypting => self.finish_encrypt(progress),
                KeyState::Decrypting => self.finish_decrypt(progress),
            });
            if let Err(error) = result {
                self.paths().key.set(None);
                return Err(error);
            }
        }
        self.finish_open();
        Ok(())
    }

    /// Open the catalogue with whatever form it is in: keyed when it is sealed,
    /// plain when a conversion has not reached it yet.
    fn open_catalogue(&self) -> Result<()> {
        let path = self.paths().db().to_path_buf();
        let conn = if db::is_plain_file(&path) {
            db::open(&path, None)?
        } else {
            self.paths().key.with(|key| db::open(&path, key))?
        };
        *self.conn() = conn;
        Ok(())
    }

    /// Seal what a conversion has not sealed yet. Safe to call again.
    fn finish_encrypt(&self, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        self.database_to_sealed()?;
        self.files_to_sealed(progress)?;
        let file = applock::read(self.root())?
            .ok_or_else(|| Error::msg("the library's key file disappeared"))?;
        applock::write(
            self.root(),
            &KeyFile {
                state: KeyState::Sealed,
                lock: file.lock,
            },
        )
    }

    /// Unseal what a conversion has not unsealed yet, then drop the key file.
    /// Safe to call again.
    fn finish_decrypt(&self, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        self.files_to_plain(progress)?;
        self.database_to_plain()?;
        applock::remove(self.root())?;
        self.paths().key.set(None);
        Ok(())
    }

    /// Export the plain catalogue to a sealed copy and swap it in. Does nothing
    /// when the catalogue is already sealed.
    fn database_to_sealed(&self) -> Result<()> {
        let path = self.paths().db().to_path_buf();
        if !db::is_plain_file(&path) {
            return Ok(());
        }
        let temp = path.with_extension("db.sealing");
        let _ = std::fs::remove_file(&temp);
        let mut conn = self.conn();
        let key_literal = self
            .paths()
            .key
            .with(|key| key.map(|key| key.hex_key()))
            .ok_or_else(|| Error::msg("the library key is not loaded"))?;
        export_database(&conn, &temp, key_literal.as_str())?;
        *conn = Connection::open_in_memory()?;
        swap_database(&path, &temp)?;
        *conn = self.paths().key.with(|key| db::open(&path, key))?;
        Ok(())
    }

    /// Export the sealed catalogue to a plain copy and swap it in. Does nothing
    /// when the catalogue is already plain.
    fn database_to_plain(&self) -> Result<()> {
        let path = self.paths().db().to_path_buf();
        if db::is_plain_file(&path) {
            return Ok(());
        }
        let temp = path.with_extension("db.plaintext");
        let _ = std::fs::remove_file(&temp);
        let mut conn = self.conn();
        export_database(&conn, &temp, "")?;
        *conn = Connection::open_in_memory()?;
        swap_database(&path, &temp)?;
        *conn = db::open(&path, None)?;
        Ok(())
    }

    /// Seal every plain file under `store/` and `thumbs/`.
    fn files_to_sealed(&self, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let files = library_files(self.paths().root())?;
        let total = files.len();
        for (index, rel) in files.iter().enumerate() {
            if !sealed::is_sealed_file(&self.paths().abs(rel)) {
                sealed::seal_into(self.paths(), &self.paths().abs(rel), rel)?;
            }
            progress(Progress {
                done: index + 1,
                total,
            });
        }
        Ok(())
    }

    /// Unseal every sealed file under `store/` and `thumbs/`.
    fn files_to_plain(&self, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let files = library_files(self.paths().root())?;
        let total = files.len();
        for (index, rel) in files.iter().enumerate() {
            if sealed::is_sealed_file(&self.paths().abs(rel)) {
                sealed::unseal_in_place(self.paths(), rel)?;
            }
            progress(Progress {
                done: index + 1,
                total,
            });
        }
        Ok(())
    }
}

/// Export the open catalogue into a new database file at `temp`, keyed with
/// `key_literal` (empty for plain), and carry its version numbers across.
fn export_database(conn: &Connection, temp: &Path, key_literal: &str) -> Result<()> {
    use rusqlite::limits::Limit;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let application: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    // The catalogue allows no attached databases in normal use. The export
    // needs exactly one, for as long as it runs.
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1);
    let result = (|| -> Result<()> {
        conn.execute(
            "ATTACH DATABASE ?1 AS converted KEY ?2",
            params![temp.to_string_lossy(), key_literal],
        )?;
        let exported = conn.execute_batch(&format!(
            "SELECT sqlcipher_export('converted');
             PRAGMA converted.user_version = {version};
             PRAGMA converted.application_id = {application};"
        ));
        let detached = conn.execute_batch("DETACH DATABASE converted;");
        exported?;
        detached?;
        Ok(())
    })();
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
    result
}

/// Replace the catalogue at `path` with `temp`. The write-ahead log belongs to
/// the old file, so it goes with it, and nothing is left that could replay onto
/// the new one.
fn swap_database(path: &Path, temp: &Path) -> Result<()> {
    let _ = std::fs::remove_file(sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sidecar(path, "-shm"));
    std::fs::rename(temp, path)
        .map_err(|error| Error::msg(format!("could not replace the catalogue: {error}")))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The catalogue-relative paths of every file a conversion covers, in a
/// stable order.
fn library_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for dir in ["store", "thumbs"] {
        let base = root.join(dir);
        if !base.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&base).follow_links(false) {
            let entry = entry.map_err(|error| Error::msg(error.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| Error::msg("a library file lies outside the library"))?;
            let rel = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push(rel);
        }
    }
    files.sort();
    Ok(files)
}
