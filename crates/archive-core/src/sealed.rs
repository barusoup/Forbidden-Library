//! Sealed files: every file a library keeps is encrypted under the library key
//! when the library has one.
//!
//! A plain library keeps its files as they are. A sealed library stores each
//! file as a stream written by [`crypto::SealWriter`], so its plaintext is
//! never on disk after the file is written. Everything else in this module is
//! there so the rest of the engine does not care which of the two it has:
//!
//! - Reads that need the bytes use [`read_bytes`] or [`hash_stored`].
//! - Consumers that only accept a path (FFmpeg, the JPEG XL and PNG tools)
//!   get a [`Plain`] file: the file itself for a plain library, or a
//!   decrypted copy in `tmp/` for a sealed one. The copy is deleted when the
//!   `Plain` is dropped.
//! - Anything a tool or encoder writes as a plain file goes into place with
//!   [`seal_into`], which seals it (or just moves it, for a plain library).
//!
//! The context bound into each file is the same for every file. A file's
//! name is not known while an import is still hashing it, so a per-file
//! context cannot be bound at write time; the catalogue checksum catches a
//! file swapped for another.

use crate::crypto::{self, Key, Purpose, SealWriter};
use crate::error::{Error, Result};
use crate::paths::Paths;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const CONTEXT: &[u8] = b"forbidden library file v1";

/// Where a sealed library's key lives. Shared by every clone of the paths, so
/// setting the key once is seen everywhere.
#[derive(Clone, Default)]
pub(crate) struct KeySlot(std::sync::Arc<std::sync::RwLock<Option<Key>>>);

impl std::fmt::Debug for KeySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("KeySlot")
            .field("sealed", &self.is_set())
            .finish()
    }
}

impl KeySlot {
    pub(crate) fn is_set(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
    }

    pub(crate) fn set(&self, key: Option<Key>) {
        *self.0.write().unwrap_or_else(|poison| poison.into_inner()) = key;
    }

    /// Run `f` with the key, or `None` for a plain library.
    pub(crate) fn with<R>(&self, f: impl FnOnce(Option<&Key>) -> R) -> R {
        let guard = self.0.read().unwrap_or_else(|poison| poison.into_inner());
        f(guard.as_ref())
    }
}

/// The write side of a file: plain, or sealed as it is written.
pub(crate) enum Sink {
    Plain(BufWriter<File>),
    Sealed(SealWriter<BufWriter<File>>),
}

impl Sink {
    /// Create `path` for writing. It must not already exist.
    pub(crate) fn create(path: &Path, key: Option<&Key>) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| Error::Msg(format!("create {}: {error}", path.display())))?;
        Ok(match key {
            None => Sink::Plain(BufWriter::with_capacity(1 << 20, file)),
            Some(key) => Sink::Sealed(SealWriter::new(
                key,
                Purpose::File,
                CONTEXT,
                BufWriter::with_capacity(1 << 20, file),
            )?),
        })
    }

    /// Write the final bytes, then fsync, so the file is durable before any
    /// rename that makes it visible.
    pub(crate) fn finish(self) -> Result<()> {
        let buffered = match self {
            Sink::Plain(mut writer) => {
                writer.flush()?;
                writer
            }
            Sink::Sealed(writer) => writer.finish()?,
        };
        let file = buffered
            .into_inner()
            .map_err(|error| Error::Msg(format!("flush: {}", error.error())))?;
        file.sync_all()?;
        Ok(())
    }
}

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(writer) => writer.write(data),
            Sink::Sealed(writer) => writer.write(data),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(writer) => writer.flush(),
            Sink::Sealed(writer) => writer.flush(),
        }
    }
}

/// A fresh name in `tmp/` with the extension `ext`, so tools that look at the
/// extension still see one.
pub(crate) fn temp_file(paths: &Paths, ext: &str) -> PathBuf {
    let ext = if ext.is_empty() { "bin" } else { ext };
    paths
        .tmp()
        .join(format!("{}.{}", crate::time::stamp(), ext))
}

/// The tmp name for a file that will be installed at `rel`.
pub(crate) fn temp_name(paths: &Paths, rel: &str) -> PathBuf {
    temp_file(paths, extension_of(rel))
}

fn extension_of(rel: &str) -> &str {
    Path::new(rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin")
}

/// The plaintext of a library file, for a consumer that needs a path. For a
/// sealed file this is a decrypted copy, deleted when the value is dropped.
pub struct Plain {
    path: PathBuf,
    temp: bool,
}

impl Plain {
    /// The plaintext file. Valid for as long as this value lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Plain {
    fn drop(&mut self) {
        if self.temp {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The plaintext of `rel` as a file on disk. See [`Plain`].
pub(crate) fn plain_path(paths: &Paths, rel: &str) -> Result<Plain> {
    plain_of(paths, &paths.abs(rel), extension_of(rel))
}

/// The plaintext of the file at `source`, which may be anywhere in the library
/// (a staged file in `tmp/`, say). `ext` names the temporary copy's extension.
pub(crate) fn plain_of(paths: &Paths, source: &Path, ext: &str) -> Result<Plain> {
    paths.key.with(|key| match key {
        None => Ok(Plain {
            path: source.to_path_buf(),
            temp: false,
        }),
        Some(key) => {
            let path = temp_file(paths, ext);
            if let Err(error) = decrypt_to(key, source, &path) {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            Ok(Plain { path, temp: true })
        }
    })
}

fn decrypt_to(key: &Key, source: &Path, dest: &Path) -> Result<()> {
    let input = BufReader::new(open(source)?);
    let mut output = BufWriter::with_capacity(1 << 20, File::create(dest)?);
    crypto::open_stream(key, Purpose::File, CONTEXT, input, &mut output)
        .map_err(|error| Error::Msg(format!("{}: {error}", source.display())))?;
    output.flush()?;
    output
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    Ok(())
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| Error::Msg(format!("open {}: {error}", path.display())))
}

/// The whole plaintext of `rel`, in memory. Meant for files that are small
/// enough to decode in memory, such as pictures.
pub(crate) fn read_bytes(paths: &Paths, rel: &str) -> Result<Vec<u8>> {
    let source = paths.abs(rel);
    paths.key.with(|key| match key {
        None => Ok(std::fs::read(&source)?),
        Some(key) => {
            let mut plain = Vec::new();
            crypto::open_stream(
                key,
                Purpose::File,
                CONTEXT,
                BufReader::new(open(&source)?),
                &mut plain,
            )
            .map_err(|error| Error::Msg(format!("{rel}: {error}")))?;
            Ok(plain)
        }
    })
}

/// The BLAKE3 hash of the plaintext of `rel`, computed without keeping it.
pub(crate) fn hash_stored(paths: &Paths, rel: &str) -> Result<String> {
    let source = paths.abs(rel);
    paths.key.with(|key| match key {
        None => crate::store::hash_file(&source),
        Some(key) => {
            let mut hasher = HashWriter(blake3::Hasher::new());
            crypto::open_stream(
                key,
                Purpose::File,
                CONTEXT,
                BufReader::new(open(&source)?),
                &mut hasher,
            )
            .map_err(|error| Error::Msg(format!("{rel}: {error}")))?;
            Ok(hasher.0.finalize().to_hex().to_string())
        }
    })
}

struct HashWriter(blake3::Hasher);

impl Write for HashWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.update(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Put the plain file `plain` in place as `rel`. For a sealed library it is
/// sealed on the way, and the plain file is removed; for a plain library it
/// is moved. An existing file at `rel` is replaced.
pub(crate) fn seal_into(paths: &Paths, plain: &Path, rel: &str) -> Result<()> {
    let target = paths.abs(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sealed = paths.key.is_set();
    if !sealed {
        std::fs::rename(plain, &target)?;
        return Ok(());
    }
    let staged = temp_name(paths, rel);
    let result = paths.key.with(|key| -> Result<()> {
        let key = key.expect("sealed library has its key");
        let mut input = BufReader::new(open(plain)?);
        let mut sink = Sink::create(&staged, Some(key))?;
        std::io::copy(&mut input, &mut sink)?;
        sink.finish()
    });
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    let _ = std::fs::remove_file(plain);
    std::fs::rename(&staged, &target).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        Error::Msg(format!("move {} into place: {error}", target.display()))
    })
}

/// Seal `plain` into a new tmp file with the extension `ext` and return its
/// path, leaving `plain` where it was. For a plain library the copy is still
/// made, so the caller can treat both alike.
pub(crate) fn seal_to_temp(paths: &Paths, plain: &Path, ext: &str) -> Result<PathBuf> {
    let staged = temp_file(paths, ext);
    let result = paths.key.with(|key| -> Result<()> {
        let mut input = BufReader::new(open(plain)?);
        let mut sink = Sink::create(&staged, key)?;
        std::io::copy(&mut input, &mut sink)?;
        sink.finish()
    });
    match result {
        Ok(()) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

/// Write `bytes` as the library file `rel`, sealed when the library is.
pub(crate) fn write_bytes(paths: &Paths, rel: &str, bytes: &[u8]) -> Result<()> {
    let target = paths.abs(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = temp_name(paths, rel);
    let result = paths.key.with(|key| -> Result<()> {
        let mut sink = Sink::create(&staged, key)?;
        sink.write_all(bytes)?;
        sink.finish()
    });
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    std::fs::rename(&staged, &target).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        Error::Msg(format!("move {} into place: {error}", target.display()))
    })
}

/// Whether the file at `path` is a sealed library file, judged by its header
/// alone. Used when converting a library, so a file is not sealed twice.
pub(crate) fn is_sealed_file(path: &Path) -> bool {
    let mut header = [0u8; 4 + 1 + 2];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let context_len = u16::from_le_bytes([header[5], header[6]]) as usize;
    if &header[..4] != b"FLV1" || header[4] != Purpose::File as u8 || context_len != CONTEXT.len() {
        return false;
    }
    let mut stored = vec![0u8; context_len];
    file.read_exact(&mut stored).is_ok() && stored == CONTEXT
}

/// Unseal `rel` in place: the sealed file becomes plain. Used when a sealed
/// library is converted back to plain.
pub(crate) fn unseal_in_place(paths: &Paths, rel: &str) -> Result<()> {
    let target = paths.abs(rel);
    let staged = temp_name(paths, rel);
    let result = paths.key.with(|key| -> Result<()> {
        let key = key.ok_or_else(|| Error::msg("the library key is not loaded"))?;
        decrypt_to(key, &target, &staged)
    });
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    std::fs::rename(&staged, &target).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        Error::Msg(format!("move {} into place: {error}", target.display()))
    })
}
