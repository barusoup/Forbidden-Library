use crate::db;
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::sealed;
use rusqlite::Connection;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One megabyte per read: deep enough that syscall overhead is not a factor
/// on a large file, small enough to stay in cache. The buffer is allocated
/// once per thread and reused, because zeroing a fresh multi-megabyte buffer
/// for every file cost more than copying a small one.
const COPY_BUFFER: usize = 1024 * 1024;

thread_local! {
    static BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `f` with this thread's read buffer, allocating it on first use.
fn with_buffer<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    BUFFER.with(|cell| {
        let mut buffer = cell.borrow_mut();
        if buffer.len() < COPY_BUFFER {
            buffer.resize(COPY_BUFFER, 0);
        }
        f(&mut buffer[..COPY_BUFFER])
    })
}

/// A file copied into staging, hashed over exactly the bytes written.
pub struct Staged {
    pub hash: String,
    pub tmp: PathBuf,
    pub bytes: u64,
}

impl Staged {
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// Copy `source` into `tmp/`, hashing as it goes. The staging file is sealed
/// on the way when the library is sealed, and is fsynced before this returns,
/// so its bytes are durable before any rename. The hash is always of the
/// plaintext. `progress` receives the number of bytes copied so far.
pub fn stage_copy(paths: &Paths, source: &Path, mut progress: impl FnMut(u64)) -> Result<Staged> {
    let mut input = open_sequential(source)
        .map_err(|e| Error::Msg(format!("open {}: {e}", source.display())))?;
    let tmp = sealed::temp_file(paths, "stage");
    let mut output = paths.key.with(|key| sealed::Sink::create(&tmp, key))?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    let result = with_buffer(|buffer| -> Result<()> {
        loop {
            let read = input.read(buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
            total += read as u64;
            progress(total);
        }
        Ok(())
    });
    if let Err(error) = result {
        drop(output);
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = output.finish() {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(Staged {
        hash: hasher.finalize().to_hex().to_string(),
        tmp,
        bytes: total,
    })
}

/// Swap a staged copy for a replacement encoded beside it, at the plain path
/// `replacement`. The replacement is hashed and fsynced, and sealed into tmp
/// when the library is sealed, before anything else happens; a failure leaves
/// the original staged copy where it was. Only then is the original removed.
pub fn replace_staged(paths: &Paths, original: &Staged, replacement: &Path) -> Result<Staged> {
    let hash = hash_file(replacement)?;
    let bytes = std::fs::metadata(replacement)?.len();
    let tmp = if paths.key.is_set() {
        let ext = replacement
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("bin")
            .to_owned();
        let sealed_tmp = sealed::seal_to_temp(paths, replacement, &ext)?;
        let _ = std::fs::remove_file(replacement);
        sealed_tmp
    } else {
        OpenOptions::new()
            .write(true)
            .open(replacement)?
            .sync_all()?;
        replacement.to_path_buf()
    };
    original.discard();
    Ok(Staged { hash, tmp, bytes })
}

/// Move a staged file into the store. Returns the catalogue-relative path.
/// If an identical file is already stored the staged copy is dropped.
pub fn install(paths: &Paths, staged: &Staged, ext: &str) -> Result<String> {
    let rel = Paths::store_rel(&staged.hash, ext);
    let target = paths.abs(&rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if target.exists() {
        staged.discard();
        return Ok(rel);
    }
    match std::fs::rename(&staged.tmp, &target) {
        Ok(()) => Ok(rel),
        Err(error) => {
            // A concurrent import may have won the race; that is a dedupe,
            // not a failure.
            if target.exists() {
                staged.discard();
                return Ok(rel);
            }
            staged.discard();
            Err(Error::Msg(format!(
                "move staged file into store ({} -> {}): {error}",
                staged.tmp.display(),
                target.display()
            )))
        }
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = open_sequential(path)?;
    let mut hasher = blake3::Hasher::new();
    with_buffer(|buffer| -> std::io::Result<()> {
        loop {
            let read = file.read(buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(())
    })?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Best-effort deletion of a store file that no catalogue row references.
///
/// The reference check is made here rather than by the caller, so no caller
/// can delete a file that a committed item still points at. A failed check
/// keeps the file: an orphan costs disk, a missing file costs an item.
pub fn remove_unreferenced(paths: &Paths, conn: &Connection, rel: &str) {
    if matches!(db::path_references(conn, rel), Ok(0)) {
        let _ = std::fs::remove_file(paths.abs(rel));
    }
}

/// Open a file for a straight read-through. Windows caches and read-aheads
/// differently when it knows the access pattern, which is worth a few per
/// cent on a multi-gigabyte import.
fn open_sequential(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    options.open(path)
}
