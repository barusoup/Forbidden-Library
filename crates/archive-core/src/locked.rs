//! Password locks: sealing a collection's items into `vault/`, and opening
//! them again.
//!
//! A collection is in one of three states:
//!
//! - **Ordinary.** No lock fields. Its items are ordinary items.
//! - **Locked.** Lock fields are set and `vault_open` is 0. The items are not
//!   in `items` at all. Their catalogue rows and their files, encrypted, live in
//!   `vault/<id>/<seal>/`, and nothing about them is readable without the
//!   password.
//! - **Unlocked.** Lock fields are set, `vault_open` is 1, and this session
//!   holds the data key. The items are in `items` with `vault_id` set, so every
//!   other page leaves them out, and their files are plaintext in `store/`.
//!
//! Every change of state is one catalogue transaction. Plaintext files that a
//! change makes unnecessary are listed in `vault_purge` in that same
//! transaction, and deleted afterwards. A crash can therefore leave a file
//! behind, but never without a record of it, and the next open removes it.
//!
//! A crash while a collection is unlocked leaves its files plaintext on disk
//! until it is unlocked and locked again, because the data key is not kept
//! anywhere that survives the process.

use crate::Library;
use crate::crypto::{self, DEFAULT_KDF, Kdf, Key, MIN_PASSWORD_CHARS, Purpose};
use crate::db;
use crate::error::{Error, Result};
use crate::model::LockState;
use crate::paths::Paths;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

const MANIFEST_FILE: &str = "manifest.sealed";
/// The tables a locked item's rows come from, in the order they are restored.
/// A manifest naming any other table is refused, whatever its authentication.
const ITEM_TABLES: [&str; 5] = [
    "items",
    "sources",
    "audio_tags",
    "lyrics",
    "collection_items",
];

/// One catalogue row, as the locked collection holds it.
#[derive(Serialize, Deserialize)]
struct Row {
    table: String,
    columns: Vec<String>,
    values: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct SealedItem {
    id: String,
    /// Where the original lives in the store. Restored to the same place.
    rel_path: String,
    checksum: String,
    /// The sealed original's name in the seal folder. `None` when the original
    /// was already missing from the store, and is restored missing again.
    blob: Option<String>,
    /// Cached grid/viewer images, encrypted separately so unlocking does not
    /// have to decode and resize every original again. Older seals omit this.
    #[serde(default)]
    previews: Vec<SealedPreview>,
    rows: Vec<Row>,
}

#[derive(Serialize, Deserialize)]
struct SealedPreview {
    /// Index into Paths::preview_rel_paths; never an arbitrary output path.
    slot: usize,
    blob: String,
}

/// What a sealed collection holds. Sealed under the data key, so its item list
/// is hidden along with the files.
#[derive(Serialize, Deserialize)]
struct Manifest {
    schema: i64,
    collection: i64,
    items: Vec<SealedItem>,
}

/// A lock's catalogue record.
pub(crate) struct LockRecord {
    kdf: String,
    salt: Vec<u8>,
    wrapped: Vec<u8>,
    open: bool,
    seal: Option<String>,
}

/// Check a password before anything is derived from it.
pub fn check_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(Error::msg(format!(
            "use at least {MIN_PASSWORD_CHARS} characters for the password"
        )));
    }
    Ok(())
}

fn wrap_context(id: i64) -> Vec<u8> {
    format!("collection {id} key").into_bytes()
}

fn blob_context(id: i64, seal: &str, blob: &str) -> Vec<u8> {
    format!("collection {id} seal {seal} blob {blob}").into_bytes()
}

fn manifest_context(id: i64, seal: &str) -> Vec<u8> {
    format!("collection {id} seal {seal} manifest").into_bytes()
}

fn preview_context(id: i64, seal: &str, item: &str, slot: usize, blob: &str) -> Vec<u8> {
    format!("collection {id} seal {seal} preview {item} {slot} {blob}").into_bytes()
}

/// A random name for a folder or file: 128 bits, hex. Reveals nothing about
/// the item it names.
fn random_name() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::msg(format!("no randomness is available: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn to_json(value: ValueRef<'_>) -> Result<serde_json::Value> {
    Ok(match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(number) => serde_json::Value::from(number),
        ValueRef::Real(number) => serde_json::Number::from_f64(number)
            .map(serde_json::Value::Number)
            .ok_or_else(|| Error::msg("a locked item holds a number that cannot be stored"))?,
        ValueRef::Text(text) => {
            serde_json::Value::String(String::from_utf8_lossy(text).into_owned())
        }
        ValueRef::Blob(_) => {
            return Err(Error::msg(
                "a locked item holds binary data that cannot be sealed",
            ));
        }
    })
}

fn from_json(value: &serde_json::Value) -> Result<Value> {
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => Value::Integer(integer),
            (None, Some(real)) => Value::Real(real),
            (None, None) => return Err(Error::msg("the lock holds an unreadable number")),
        },
        serde_json::Value::String(text) => Value::Text(text.clone()),
        _ => return Err(Error::msg("the lock holds an unreadable value")),
    })
}

/// Every row matching `clause`, with its column names, read whole. Generic
/// over the schema so that a locked item keeps every column it has, including
/// ones added later.
fn export(conn: &Connection, table: &str, clause: &str, args: &[&dyn ToSql]) -> Result<Vec<Row>> {
    let sql = format!("SELECT * FROM {table} WHERE {clause}");
    let mut stmt = conn.prepare(&sql)?;
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut rows = stmt.query(args)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(to_json(row.get_ref(index)?)?);
        }
        out.push(Row {
            table: table.to_string(),
            columns: columns.clone(),
            values,
        });
    }
    Ok(out)
}

fn insert(conn: &Connection, row: &Row) -> Result<()> {
    if !ITEM_TABLES.contains(&row.table.as_str())
        || row.columns.len() != row.values.len()
        || !row.columns.iter().all(|column| {
            column
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    {
        return Err(Error::msg(
            "the lock holds a row that does not belong to a collection",
        ));
    }
    let placeholders = vec!["?"; row.columns.len()].join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({placeholders})",
        row.table,
        row.columns.join(", ")
    );
    let values = row
        .values
        .iter()
        .map(from_json)
        .collect::<Result<Vec<Value>>>()?;
    conn.execute(&sql, params_from_iter(values.iter()))?;
    Ok(())
}

/// Hashes what passes through it, so a restored original can be checked
/// against the checksum its row recorded.
struct Hashing<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write `data` sealed to `path`: staged beside it, fsynced, then renamed, so
/// a reader never sees a half-written manifest.
fn write_sealed_file(
    key: &Key,
    purpose: Purpose,
    context: &[u8],
    data: impl Read,
    path: &Path,
) -> Result<()> {
    let staged = path.with_extension("partial");
    let file = File::create(&staged)?;
    let mut out = BufWriter::new(file);
    crypto::seal_stream(key, purpose, context, data, &mut out)?;
    out.flush()?;
    out.get_ref().sync_all()?;
    drop(out);
    std::fs::rename(&staged, path)?;
    Ok(())
}

impl Library {
    /// Whether this session holds the key for a locked collection, so its items
    /// can be read and changed.
    pub fn is_unlocked(&self, id: i64) -> bool {
        self.vault_keys_guard().contains_key(&id)
    }

    /// The lock a collection is in right now. A collection that was unlocked
    /// when the app last closed reads as locked until it is unlocked again.
    pub fn lock_state(&self, id: i64) -> Result<LockState> {
        let locked = db::collection_lock_state(&self.conn(), id)?;
        Ok(match locked {
            None | Some((false, _)) => LockState::None,
            Some((true, _)) if self.is_unlocked(id) => LockState::Unlocked,
            Some((true, _)) => LockState::Locked,
        })
    }

    /// Put an ordinary collection behind a password. Its items leave every
    /// other collection and every page, and are sealed before this returns, so
    /// the collection is locked on success.
    pub fn create_lock(&self, id: i64, password: &str) -> Result<()> {
        check_password(password)?;
        let _files = crate::write_gate(&self.files);
        match db::collection_lock_state(&self.conn(), id)? {
            None => return Err(Error::msg("that collection no longer exists")),
            Some((true, _)) => return Err(Error::msg("this collection is already locked")),
            Some((false, _)) => {}
        }
        let salt = crypto::new_salt()?;
        let kek = crypto::derive(password, &salt, DEFAULT_KDF)?;
        let data_key = Key::random()?;
        let wrapped = crypto::wrap(&kek, &data_key, &wrap_context(id))?;
        drop(kek);
        {
            let conn = self.conn();
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE collections SET vault_kdf = ?2, vault_salt = ?3, vault_wrap = ?4,
                    vault_open = 1, vault_seal = NULL
                 WHERE id = ?1",
                params![id, DEFAULT_KDF.encode(), salt.as_slice(), wrapped],
            )?;
            // Trashed items stay in the library's trash: a locked collection has no
            // trash of its own, so they leave it rather than vanish inside it.
            tx.execute(
                "DELETE FROM collection_items WHERE collection_id = ?1
                 AND item_id IN (SELECT id FROM items WHERE deleted_at IS NOT NULL)",
                params![id],
            )?;
            // A member's other collections drop it, and its vectors go with it.
            tx.execute(
                "DELETE FROM collection_items WHERE collection_id <> ?1
                 AND item_id IN (SELECT item_id FROM collection_items WHERE collection_id = ?1)",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM item_vectors
                 WHERE item_id IN (SELECT item_id FROM collection_items WHERE collection_id = ?1)",
                params![id],
            )?;
            tx.execute(
                "UPDATE items SET vault_id = ?1
                 WHERE id IN (SELECT item_id FROM collection_items WHERE collection_id = ?1)",
                params![id],
            )?;
            tx.commit()?;
        }
        self.vault_keys_guard().insert(id, data_key);
        // A lock that cannot seal leaves the collection unlocked, so the
        // error is shown and the user can try again.
        self.seal_unlocked(id)
    }

    /// Unlock a locked collection for this session. A wrong password changes
    /// nothing. The key is held only until the collection is locked again or
    /// the app closes.
    pub fn unlock(&self, id: i64, password: &str) -> Result<()> {
        if self.is_unlocked(id) {
            return Ok(());
        }
        let record = self
            .lock_record(id)?
            .ok_or_else(|| Error::msg("this collection is not locked"))?;
        let kdf = Kdf::decode(&record.kdf)?;
        let kek = crypto::derive(password, &record.salt, kdf)?;
        let data_key = crypto::unwrap(&kek, &record.wrapped, &wrap_context(id))?;
        drop(kek);
        let _files = crate::write_gate(&self.files);
        if self.is_unlocked(id) {
            return Ok(());
        }
        let current = self
            .lock_record(id)?
            .ok_or_else(|| Error::msg("this collection is no longer locked"))?;
        if current.kdf != record.kdf
            || current.salt != record.salt
            || current.wrapped != record.wrapped
        {
            return Err(Error::msg(
                "the collection lock changed while unlocking; try again",
            ));
        }
        if !current.open {
            let seal = current
                .seal
                .as_deref()
                .ok_or_else(|| Error::msg("the sealed copy of this collection is missing"))?;
            self.restore_sealed(id, &data_key, seal)?;
        }
        self.vault_keys_guard().insert(id, data_key);
        Ok(())
    }

    /// Seal an unlocked collection again. Does nothing when it is not unlocked.
    pub fn lock(&self, id: i64) -> Result<()> {
        let _files = crate::write_gate(&self.files);
        if !self.is_unlocked(id) {
            return Ok(());
        }
        self.seal_unlocked(id)
    }

    /// Seal every unlocked collection. Called when the app closes and when it
    /// has been left idle. Each collection is attempted even if another fails.
    pub fn lock_all(&self) -> Result<usize> {
        let ids: Vec<i64> = self.vault_keys_guard().keys().copied().collect();
        let mut first_error = None;
        for id in &ids {
            if let Err(error) = self.lock(*id) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(ids.len()),
        }
    }

    /// Remove a lock: the collection becomes ordinary again and its items return
    /// to the library. It must be unlocked first.
    pub fn remove_lock(&self, id: i64) -> Result<()> {
        let _files = crate::write_gate(&self.files);
        self.remove_lock_unlocked(id)
    }

    pub(crate) fn remove_lock_unlocked(&self, id: i64) -> Result<()> {
        if !self.is_unlocked(id) {
            return Err(Error::msg("unlock this collection first"));
        }
        {
            let conn = self.conn();
            db::release_locked(&conn, id)?;
        }
        self.vault_keys_guard().remove(&id);
        let _ = std::fs::remove_dir_all(self.paths.vault_dir(id));
        Ok(())
    }

    /// Items among `ids` the library may show. Those inside a locked collection
    /// are dropped.
    pub fn visible_ids(&self, ids: &[String]) -> Result<Vec<String>> {
        db::visible_ids(&self.conn(), ids)
    }

    /// Remove plaintext files the catalogue no longer needs. A file still
    /// referenced by an item stays, and only its entry is cleared.
    pub(crate) fn purge_pending(&self) -> Result<usize> {
        let pending: Vec<(String, String)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare("SELECT path, item_id FROM vault_purge")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut cleared = 0;
        for (path, item_id) in pending {
            let referenced: i64 = self.conn().query_row(
                "SELECT COUNT(*) FROM items WHERE rel_path = ?1 OR (?3 AND id = ?2)",
                params![
                    path,
                    item_id,
                    Paths::preview_rel_paths(&item_id).contains(&path)
                ],
                |row| row.get(0),
            )?;
            if !path.starts_with("store/") && !path.starts_with("thumbs/") {
                return Err(Error::msg("unsafe plaintext purge path"));
            }
            let owned = self.paths.checked_owned_path(&path)?;
            let done = referenced > 0
                || match shred(&owned) {
                    Ok(()) => true,
                    Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                };
            if done {
                self.conn()
                    .execute("DELETE FROM vault_purge WHERE path = ?1", params![path])?;
                cleared += 1;
            }
        }
        Ok(cleared)
    }

    /// Startup housekeeping: finish any plaintext purge a crash interrupted,
    /// and delete sealed folders that no lock refers to.
    pub(crate) fn reconcile_vault(&self) -> Result<()> {
        self.purge_pending()?;
        let root = self.paths.vault();
        if !root.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            let id = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<i64>().ok());
            let keep = match id {
                Some(id) => match self.lock_record(id)? {
                    Some(record) if !record.open => record.seal,
                    _ => None,
                },
                None => None,
            };
            let Some(keep) = keep else {
                remove_any(&path);
                continue;
            };
            for sealed in std::fs::read_dir(&path)? {
                let sealed = sealed?.path();
                if sealed.file_name().and_then(|name| name.to_str()) != Some(keep.as_str()) {
                    remove_any(&sealed);
                }
            }
        }
        Ok(())
    }

    fn lock_record(&self, id: i64) -> Result<Option<LockRecord>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT vault_kdf, vault_salt, vault_wrap, vault_open, vault_seal
                 FROM collections WHERE id = ?1 AND vault_kdf IS NOT NULL",
                params![id],
                |row| {
                    Ok(LockRecord {
                        kdf: row.get(0)?,
                        salt: row.get(1)?,
                        wrapped: row.get(2)?,
                        open: row.get(3)?,
                        seal: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn vault_keys_guard(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<i64, Key>> {
        crate::lock_mutex(&self.vault_keys)
    }

    /// Seal a collection whose key this session holds, and drop the key. The
    /// caller holds the file gate. On failure the key is kept, so the collection
    /// stays unlocked and the attempt can be repeated.
    fn seal_unlocked(&self, id: i64) -> Result<()> {
        let key = self
            .vault_keys_guard()
            .remove(&id)
            .ok_or_else(|| Error::msg("unlock this collection first"))?;
        match self.seal(id, &key) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Housekeeping can fail after the catalogue commit. A committed
                // seal is locked even if a pending plaintext purge needs retry.
                if self.lock_record(id)?.is_none_or(|record| record.open) {
                    self.vault_keys_guard().insert(id, key);
                }
                Err(error)
            }
        }
    }

    fn seal(&self, id: i64, key: &Key) -> Result<()> {
        let seal = random_name()?;
        let dir = self
            .paths
            .checked_owned_path(&format!("vault/{id}/{seal}"))?;
        std::fs::create_dir_all(&dir)?;
        let outcome = self.write_seal(id, key, &seal, &dir);
        if outcome.is_err()
            && self
                .lock_record(id)?
                .is_none_or(|record| record.seal.as_deref() != Some(&seal))
        {
            let _ = std::fs::remove_dir_all(&dir);
        }
        outcome
    }

    fn write_seal(&self, id: i64, key: &Key, seal: &str, dir: &Path) -> Result<()> {
        // Read every row first. The catalogue lock is not held across file work.
        let snapshot: Vec<(String, String, String, Vec<Row>)> = {
            let conn = self.conn();
            let ids: Vec<String> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM items WHERE vault_id = ?1 ORDER BY id")?;
                let rows = stmt.query_map(params![id], |row| row.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let mut snapshot = Vec::with_capacity(ids.len());
            for item_id in ids {
                let (rel_path, checksum): (String, String) = conn.query_row(
                    "SELECT rel_path, checksum FROM items WHERE id = ?1",
                    params![item_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let mut rows = export(&conn, "items", "id = ?1", &[&item_id])?;
                rows.extend(export(&conn, "sources", "item_id = ?1", &[&item_id])?);
                rows.extend(export(&conn, "audio_tags", "item_id = ?1", &[&item_id])?);
                rows.extend(export(&conn, "lyrics", "item_id = ?1", &[&item_id])?);
                rows.extend(export(
                    &conn,
                    "collection_items",
                    "collection_id = ?1 AND item_id = ?2",
                    &[&id, &item_id],
                )?);
                snapshot.push((item_id, rel_path, checksum, rows));
            }
            snapshot
        };

        let mut items = Vec::with_capacity(snapshot.len());
        for (item_id, rel_path, checksum, rows) in snapshot {
            let original = self.paths.checked_owned_path(&rel_path)?;
            let blob = if original.is_file() {
                let name = random_name()?;
                let source = File::open(&original)?;
                let target = dir.join(format!("{name}.blob"));
                let staged = dir.join(format!("{name}.partial"));
                let mut out = BufWriter::new(File::create(&staged)?);
                crypto::seal_stream(
                    key,
                    Purpose::Blob,
                    &blob_context(id, seal, &name),
                    source,
                    &mut out,
                )?;
                out.flush()?;
                out.get_ref().sync_all()?;
                drop(out);
                std::fs::rename(&staged, &target)?;
                Some(format!("{name}.blob"))
            } else {
                None
            };
            let mut previews = Vec::new();
            for (slot, relative) in Paths::preview_rel_paths(&item_id).iter().enumerate() {
                let path = self.paths.checked_owned_path(relative)?;
                if !path.is_file() {
                    continue;
                }
                let blob = format!("{}.preview", random_name()?);
                write_sealed_file(
                    key,
                    Purpose::Blob,
                    &preview_context(id, seal, &item_id, slot, &blob),
                    File::open(path)?,
                    &dir.join(&blob),
                )?;
                previews.push(SealedPreview { slot, blob });
            }
            items.push(SealedItem {
                id: item_id,
                rel_path,
                checksum,
                blob,
                previews,
                rows,
            });
        }

        let manifest = Manifest {
            schema: db::SCHEMA_VERSION,
            collection: id,
            items,
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        write_sealed_file(
            key,
            Purpose::Manifest,
            &manifest_context(id, seal),
            manifest_bytes.as_slice(),
            &dir.join(MANIFEST_FILE),
        )?;

        // Commit. If the collection changed while it was being sealed, nothing
        // is removed: the sealed copy is discarded and the collection stays
        // unlocked.
        let snapshot_ids: Vec<&str> = manifest.items.iter().map(|item| item.id.as_str()).collect();
        {
            let conn = self.conn();
            let current: Vec<String> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM items WHERE vault_id = ?1 ORDER BY id")?;
                let rows = stmt.query_map(params![id], |row| row.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if current
                .iter()
                .map(String::as_str)
                .ne(snapshot_ids.iter().copied())
            {
                return Err(Error::msg(
                    "the collection changed while it was being locked; it is still unlocked",
                ));
            }
            let tx = conn.unchecked_transaction()?;
            for item in &manifest.items {
                let paths = Paths::preview_rel_paths(&item.id);
                for path in
                    std::iter::once(item.rel_path.as_str()).chain(paths.iter().map(String::as_str))
                {
                    tx.execute(
                        "INSERT OR REPLACE INTO vault_purge (path, item_id) VALUES (?1, ?2)",
                        params![path, item.id],
                    )?;
                }
            }
            tx.execute("DELETE FROM items WHERE vault_id = ?1", params![id])?;
            tx.execute(
                "UPDATE collections SET vault_open = 0, vault_seal = ?2 WHERE id = ?1",
                params![id, seal],
            )?;
            tx.commit()?;
        }

        // The committed seal supersedes any earlier one.
        if let Ok(entries) = std::fs::read_dir(self.paths.vault_dir(id)) {
            for entry in entries.flatten() {
                if entry.file_name().to_str() != Some(seal) {
                    remove_any(&entry.path());
                }
            }
        }
        // Rows deleted by the commit may still sit in the write-ahead log.
        // Checkpointing and truncating it leaves no copy of them there.
        let _ = self
            .conn()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        self.purge_pending()?;
        Ok(())
    }

    /// Bring a sealed collection's files back into the store and its rows
    /// into the catalogue. The caller holds the file gate.
    fn restore_sealed(&self, id: i64, key: &Key, seal: &str) -> Result<()> {
        if seal.len() != 32 || !seal.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::msg("invalid sealed collection name"));
        }
        let dir = self
            .paths
            .checked_owned_path(&format!("vault/{id}/{seal}"))?;
        let manifest: Manifest = {
            let mut bytes = Vec::new();
            crypto::open_stream(
                key,
                Purpose::Manifest,
                &manifest_context(id, seal),
                File::open(dir.join(MANIFEST_FILE))?,
                &mut bytes,
            )?;
            serde_json::from_slice(&bytes)?
        };
        if manifest.schema != db::SCHEMA_VERSION || manifest.collection != id {
            return Err(Error::msg(
                "this lock was made by a different version of Forbidden Library",
            ));
        }

        for item in &manifest.items {
            if item.id.len() != 64
                || !item.id.bytes().all(|b| b.is_ascii_hexdigit())
                || !item.rel_path.starts_with("store/")
                || item.blob.as_ref().is_some_and(|blob| {
                    blob.len() != 37
                        || !blob.ends_with(".blob")
                        || !blob[..32].bytes().all(|b| b.is_ascii_hexdigit())
                })
            {
                return Err(Error::msg("the sealed manifest holds an unsafe path"));
            }
            self.paths.checked_owned_path(&item.rel_path)?;
            if let Some(blob) = &item.blob {
                self.paths
                    .checked_owned_path(&format!("vault/{id}/{seal}/{blob}"))?;
            }
            let paths = Paths::preview_rel_paths(&item.id);
            let mut seen = [false; 3];
            for preview in &item.previews {
                if preview.slot >= paths.len()
                    || seen[preview.slot]
                    || preview.blob.len() != 40
                    || !preview.blob.ends_with(".preview")
                    || !preview.blob[..32].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(Error::msg("the sealed manifest holds an unsafe preview"));
                }
                seen[preview.slot] = true;
                self.paths.checked_owned_path(&paths[preview.slot])?;
                self.paths
                    .checked_owned_path(&format!("vault/{id}/{seal}/{}", preview.blob))?;
            }
        }

        // Record each file before writing it, so an interrupted restore is
        // cleaned up by the next purge.
        {
            let conn = self.conn();
            let tx = conn.unchecked_transaction()?;
            for item in &manifest.items {
                let paths = Paths::preview_rel_paths(&item.id);
                for relative in item.blob.iter().map(|_| item.rel_path.as_str()).chain(
                    item.previews
                        .iter()
                        .map(|preview| paths[preview.slot].as_str()),
                ) {
                    tx.execute(
                        "INSERT OR REPLACE INTO vault_purge (path, item_id) VALUES (?1, ?2)",
                        params![relative, item.id],
                    )?;
                }
            }
            tx.commit()?;
        }
        let written: Result<()> = manifest.items.iter().try_for_each(|item| {
            if let Some(blob) = &item.blob {
                self.restore_blob(
                    key,
                    &dir.join(blob),
                    &blob_context(id, seal, blob.trim_end_matches(".blob")),
                    &item.rel_path,
                    Some(&item.checksum),
                )?;
            }
            let paths = Paths::preview_rel_paths(&item.id);
            for preview in &item.previews {
                self.restore_blob(
                    key,
                    &dir.join(&preview.blob),
                    &preview_context(id, seal, &item.id, preview.slot, &preview.blob),
                    &paths[preview.slot],
                    None,
                )?;
            }
            Ok(())
        });
        if let Err(error) = written {
            let _ = self.purge_pending();
            return Err(error);
        }

        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            for item in &manifest.items {
                // A plain copy of the same bytes (the same photo imported again while
                // the collection was locked) gives way to the locked one.
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT rel_path FROM items WHERE id = ?1",
                        params![item.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(previous) = previous.filter(|path| *path != item.rel_path) {
                    tx.execute(
                        "INSERT OR REPLACE INTO vault_purge (path, item_id) VALUES (?1, ?2)",
                        params![previous, item.id],
                    )?;
                }
                tx.execute("DELETE FROM items WHERE id = ?1", params![item.id])?;
                for row in &item.rows {
                    insert(&tx, row)?;
                }
                // Legacy seals and missing caches still rebuild on demand.
                if !item.previews.iter().any(|preview| preview.slot == 0) {
                    tx.execute(
                    "UPDATE items SET thumb_state = 'pending'
                     WHERE id = ?1 AND thumb_state <> 'none' AND kind IN ('image', 'video', 'audio')",
                    params![item.id],
                    )?;
                }
                tx.execute(
                    "DELETE FROM vault_purge WHERE path = ?1",
                    params![item.rel_path],
                )?;
                for preview in &item.previews {
                    tx.execute(
                        "DELETE FROM vault_purge WHERE path = ?1",
                        params![Paths::preview_rel_paths(&item.id)[preview.slot]],
                    )?;
                }
            }
            tx.execute(
                "UPDATE collections SET vault_open = 1, vault_seal = NULL WHERE id = ?1",
                params![id],
            )?;
            tx.commit()?;
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = self.purge_pending();
        Ok(())
    }

    fn restore_blob(
        &self,
        key: &Key,
        source: &Path,
        context: &[u8],
        relative: &str,
        checksum: Option<&str>,
    ) -> Result<()> {
        let staged = self
            .paths
            .checked_owned_path(&format!("tmp/{}.stage", random_name()?))?;
        let outcome = (|| -> Result<()> {
            let hash = {
                let mut out = Hashing {
                    inner: BufWriter::new(
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&staged)?,
                    ),
                    hasher: blake3::Hasher::new(),
                };
                crypto::open_stream(key, Purpose::Blob, context, File::open(source)?, &mut out)?;
                out.flush()?;
                out.inner.get_ref().sync_all()?;
                out.hasher.finalize().to_hex().to_string()
            };
            if checksum.is_some_and(|checksum| hash != checksum) {
                return Err(Error::msg(format!(
                    "a sealed file in this collection does not match its record ({relative})"
                )));
            }
            let target = self.paths.checked_owned_path(relative)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&staged, &target)?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        outcome
    }
}

/// Remove a file or folder, best effort: a leftover is cleared by the next purge
/// or reconcile.
fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Deletes a plaintext file after overwriting it with zeros, so its contents are
/// not left readable on the disk. A solid-state drive can keep the old blocks
/// wherever it likes, so this is as far as deleting a file can go there.
fn shred(path: &Path) -> std::io::Result<()> {
    if let Ok(mut file) = OpenOptions::new().write(true).open(path) {
        let mut remaining = file.metadata()?.len();
        let zeros = vec![0u8; 1024 * 1024];
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64) as usize;
            file.write_all(&zeros[..chunk])?;
            remaining -= chunk as u64;
        }
        file.sync_all()?;
    }
    std::fs::remove_file(path)
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::{ImportOptions, TimelineFilter, import_paths};

    fn fixture() -> (tempfile::TempDir, Library, crate::Item) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("photo.png");
        image::RgbImage::from_pixel(48, 32, image::Rgb([30, 60, 90]))
            .save(&source)
            .unwrap();
        let library = Library::open(temp.path().join("library")).unwrap();
        import_paths(&library, &[source], &ImportOptions::default(), &mut |_| {});
        let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
        (temp, library, item)
    }

    #[test]
    fn legacy_seals_without_previews_rebuild_once_then_keep_the_cache() {
        let (_temp, library, item) = fixture();
        let collection = library.create_collection("private").unwrap();
        library
            .add_to_collection(collection, std::slice::from_ref(&item.id))
            .unwrap();
        library.create_lock(collection, "test password").unwrap();
        let record = library.lock_record(collection).unwrap().unwrap();
        let kek = crypto::derive(
            "test password",
            &record.salt,
            Kdf::decode(&record.kdf).unwrap(),
        )
        .unwrap();
        let key = crypto::unwrap(&kek, &record.wrapped, &wrap_context(collection)).unwrap();
        let seal = record.seal.unwrap();
        let path = library
            .paths
            .vault_dir(collection)
            .join(&seal)
            .join(MANIFEST_FILE);
        let context = manifest_context(collection, &seal);
        let mut bytes = Vec::new();
        crypto::open_stream(
            &key,
            Purpose::Manifest,
            &context,
            File::open(&path).unwrap(),
            &mut bytes,
        )
        .unwrap();
        let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for item in manifest["items"].as_array_mut().unwrap() {
            item.as_object_mut().unwrap().remove("previews");
        }
        let bytes = serde_json::to_vec(&manifest).unwrap();
        write_sealed_file(&key, Purpose::Manifest, &context, bytes.as_slice(), &path).unwrap();

        library.unlock(collection, "test password").unwrap();
        assert_eq!(
            library.item(&item.id).unwrap().unwrap().thumb_state,
            crate::ThumbState::Pending
        );
        assert!(!library.paths.thumb_path(&item.id).exists());
        library.load_thumb_rgba(&item).unwrap();
        library.lock(collection).unwrap();
        library.unlock(collection, "test password").unwrap();
        assert_eq!(
            library.item(&item.id).unwrap().unwrap().thumb_state,
            crate::ThumbState::Ready
        );
        assert!(library.paths.thumb_path(&item.id).is_file());
    }

    #[test]
    fn stale_previews_and_late_vectors_cannot_cross_a_lock() {
        let (_temp, library, item) = fixture();
        let collection = library.create_collection("private").unwrap();
        library
            .add_to_collection(collection, std::slice::from_ref(&item.id))
            .unwrap();
        library.create_lock(collection, "test password").unwrap();
        assert!(library.make_thumbnail(&item).is_err());
        assert!(library.load_thumb_rgba(&item).is_err());
        assert!(library.load_preview_rgba(&item, 64).is_err());
        assert!(library.load_full_rgba(&item, 64).is_err());
        assert!(library.embedding_frame(&item).is_err());
        assert!(!library.paths.thumb_path(&item.id).exists());
        library.unlock(collection, "test password").unwrap();
        library
            .set_embedding(&item.id, "test", &[1.0, 0.0])
            .unwrap();
        assert_eq!(library.embedding_count("test").unwrap(), 0);
        library.remove_lock(collection).unwrap();
        library
            .set_embedding(&item.id, "test", &[f32::NAN])
            .unwrap();
        assert_eq!(library.embedding_count("test").unwrap(), 0);
        library
            .set_embedding(&item.id, "test", &[1.0, 0.0])
            .unwrap();
        assert_eq!(library.embedding_count("test").unwrap(), 1);
        library
            .delete_forever(std::slice::from_ref(&item.id))
            .unwrap();
        assert!(library.make_thumbnail(&item).is_err());
        library
            .set_embedding(&item.id, "test", &[1.0, 0.0])
            .unwrap();
        assert_eq!(library.embedding_count("test").unwrap(), 0);
    }

    #[test]
    fn concurrent_unlocks_restore_once() {
        let (_temp, library, item) = fixture();
        let collection = library.create_collection("private").unwrap();
        library
            .add_to_collection(collection, std::slice::from_ref(&item.id))
            .unwrap();
        library.create_lock(collection, "test password").unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                library.unlock(collection, "test password")
            });
            let b = scope.spawn(|| {
                barrier.wait();
                library.unlock(collection, "test password")
            });
            a.join().unwrap().unwrap();
            b.join().unwrap().unwrap();
        });
        assert!(library.is_unlocked(collection));
        assert_eq!(
            crate::store::hash_file(&library.source_path(&item)).unwrap(),
            item.checksum
        );
    }

    #[test]
    fn housekeeping_failure_never_discards_a_committed_seal() {
        let (_temp, library, item) = fixture();
        let collection = library.create_collection("private").unwrap();
        library
            .add_to_collection(collection, std::slice::from_ref(&item.id))
            .unwrap();
        library
            .conn()
            .execute(
                "INSERT INTO vault_purge(path,item_id) VALUES ('../victim',?1)",
                params![item.id],
            )
            .unwrap();
        assert!(library.create_lock(collection, "test password").is_err());
        assert_eq!(library.lock_state(collection).unwrap(), LockState::Locked);
        library
            .conn()
            .execute("DELETE FROM vault_purge WHERE path = '../victim'", [])
            .unwrap();
        library.unlock(collection, "test password").unwrap();
        assert_eq!(
            crate::store::hash_file(&library.source_path(&item)).unwrap(),
            item.checksum
        );
    }

    #[test]
    fn purge_preserves_live_previews_but_removes_old_paths_and_refuses_escapes() {
        let (temp, library, item) = fixture();
        let old = "store/obsolete.bin";
        std::fs::write(library.paths.abs(old), b"old plaintext").unwrap();
        let preview = Paths::preview_rel_paths(&item.id)[0].clone();
        for path in [old, &preview] {
            library
                .conn()
                .execute(
                    "INSERT INTO vault_purge(path,item_id) VALUES (?1,?2)",
                    params![path, item.id],
                )
                .unwrap();
        }
        library.purge_pending().unwrap();
        assert!(!library.paths.abs(old).exists());
        assert!(library.paths.thumb_path(&item.id).exists());
        let outside = temp.path().join("victim");
        std::fs::write(&outside, b"keep").unwrap();
        library
            .conn()
            .execute(
                "INSERT INTO vault_purge(path,item_id) VALUES ('../victim',?1)",
                params![item.id],
            )
            .unwrap();
        assert!(library.purge_pending().is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"keep");
    }

    #[test]
    fn collection_edits_wait_for_vault_file_gate() {
        let (_temp, library, item) = fixture();
        let collection = library.create_collection("private").unwrap();
        let gate = crate::write_gate(&library.files);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                tx.send(library.add_to_collection(collection, &[item.id]))
                    .unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(100))
                    .is_err()
            );
            drop(gate);
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap(),
                1
            );
        });
    }
}
