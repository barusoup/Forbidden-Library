//! The library password: the one password that opens a sealed library.
//!
//! A sealed library has one random 256-bit library key. The key seals every
//! file in the library and the catalogue database. It is itself wrapped by a
//! key derived from the password with Argon2id, and the wrapped copy is kept
//! in `library.key` inside the library folder. The password is never stored,
//! and a wrong one is detected because the wrapped key fails to authenticate.
//!
//! Changing the password only rewraps the library key, so nothing is
//! re-encrypted. `library.key` also records where a conversion to or from the
//! sealed form has got to, so an interrupted one resumes on the next unlock.

use crate::crypto::{self, DEFAULT_KDF, Kdf, Key};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CONTEXT: &[u8] = b"forbidden library password v1";
/// The key file's name, inside the library folder.
pub const KEY_FILE: &str = "library.key";

/// The password's record: the Argon2id settings, the salt, and the library key
/// wrapped under the password's key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLock {
    /// The Argon2id settings the password was stretched with.
    kdf: String,
    salt: Vec<u8>,
    /// The library key, wrapped under the password's key.
    wrapped: Vec<u8>,
}

/// Where a conversion of the library has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyState {
    /// Converting a plain library to sealed. The database and files may be
    /// part way through; finishing the conversion is safe to repeat.
    Encrypting,
    /// Everything is sealed.
    Sealed,
    /// Converting a sealed library back to plain. Finishing is safe to repeat.
    Decrypting,
}

/// The contents of `library.key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    pub state: KeyState,
    #[serde(flatten)]
    pub lock: AppLock,
}

/// Make a password record that wraps `key`. The password must be long enough
/// to pass the same check a collection lock applies.
pub fn wrap(key: &Key, password: &str) -> Result<AppLock> {
    crate::locked::check_password(password)?;
    let salt = crypto::new_salt()?;
    let kek = crypto::derive(password, &salt, DEFAULT_KDF)?;
    let wrapped = crypto::wrap(&kek, key, CONTEXT)?;
    Ok(AppLock {
        kdf: DEFAULT_KDF.encode(),
        salt: salt.to_vec(),
        wrapped,
    })
}

/// Make a password record around a new random key, and return that key too.
pub fn create(password: &str) -> Result<(AppLock, Key)> {
    let key = Key::random()?;
    let lock = wrap(&key, password)?;
    Ok((lock, key))
}

/// The library key, if `password` is the one `lock` was made with.
pub fn unlock(lock: &AppLock, password: &str) -> Result<Option<Key>> {
    let kdf = Kdf::decode(&lock.kdf)?;
    let kek = crypto::derive(password, &lock.salt, kdf)?;
    match crypto::unwrap(&kek, &lock.wrapped, CONTEXT) {
        Ok(key) => Ok(Some(key)),
        Err(Error::WrongPassword) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether `password` is the one `lock` was made with.
pub fn check(lock: &AppLock, password: &str) -> Result<bool> {
    Ok(unlock(lock, password)?.is_some())
}

/// Read `library.key` from a library folder. `None` for a plain library.
pub fn read(root: &Path) -> Result<Option<KeyFile>> {
    let path = root.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| Error::msg("the library's key file is damaged")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Write `library.key`. The new text goes to a temporary file first, so a
/// crash cannot leave a half-written key file.
pub fn write(root: &Path, file: &KeyFile) -> Result<()> {
    use std::io::Write;
    let path = root.join(KEY_FILE);
    let temp = root.join(format!("{KEY_FILE}.tmp"));
    let text = serde_json::to_string_pretty(file)
        .map_err(|error| Error::msg(format!("the key file could not be written: {error}")))?;
    let mut output = std::fs::File::create(&temp)?;
    output.write_all(text.as_bytes())?;
    output.sync_all()?;
    drop(output);
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// Remove `library.key`, which makes the library plain again.
pub fn remove(root: &Path) -> Result<()> {
    match std::fs::remove_file(root.join(KEY_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_password_the_record_was_made_with_opens_it() {
        let (lock, key) = create("correct horse battery").unwrap();
        let opened = unlock(&lock, "correct horse battery").unwrap().unwrap();
        assert_eq!(opened.hex_key().as_str(), key.hex_key().as_str());
        assert!(unlock(&lock, "correct horse batter").unwrap().is_none());
        assert!(!check(&lock, "").unwrap());
    }

    #[test]
    fn a_short_password_is_refused_before_anything_is_derived() {
        assert!(create("short").is_err());
    }

    #[test]
    fn rewrapping_keeps_the_same_library_key() {
        let (lock, key) = create("correct horse battery").unwrap();
        let rewrapped = wrap(&key, "another long one").unwrap();
        assert_ne!(lock, rewrapped);
        assert!(
            unlock(&rewrapped, "correct horse battery")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            unlock(&rewrapped, "another long one")
                .unwrap()
                .unwrap()
                .hex_key()
                .as_str(),
            key.hex_key().as_str()
        );
    }

    #[test]
    fn the_key_file_round_trips_through_disk() {
        let temp = tempfile::tempdir().unwrap();
        let (lock, _) = create("correct horse battery").unwrap();
        let file = KeyFile {
            state: KeyState::Encrypting,
            lock,
        };
        assert!(read(temp.path()).unwrap().is_none());
        write(temp.path(), &file).unwrap();
        assert_eq!(read(temp.path()).unwrap(), Some(file));
        remove(temp.path()).unwrap();
        assert!(read(temp.path()).unwrap().is_none());
    }
}
