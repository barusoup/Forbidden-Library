use crate::audio::{Meta as AudioMeta, Tags as AudioTags};
use crate::error::Result;
use crate::model::{
    Collection, Item, Kind, LockState, OptState, Stats, ThumbState, TimelineFilter,
};
use crate::paths::Paths;
use crate::time;
use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use std::path::Path;
use std::sync::LazyLock;

/// Identifies `library.db` as ours. A SQLite file without this id and with an
/// unknown schema is refused instead of being migrated into shape.
pub const APPLICATION_ID: i64 = 0x4D59_4152;
/// The schema this build writes. Older libraries migrate forward; newer ones
/// are refused so an old app can never damage them.
pub const SCHEMA_VERSION: i64 = 10;

/// The canonical column order every item read uses.
const ITEM_COLUMNS: &str = "\
    i.id, i.kind, i.mime, i.ext, i.original_ext, i.bytes, i.original_bytes, \
    i.width, i.height, i.duration_ms, i.captured_at, i.captured_source, \
    i.added_at, i.modified_at, i.original_name, i.rel_path, i.thumb_state, \
    i.opt_state, i.opt_kind, i.deleted_at, i.camera_make, \
    i.camera_model, i.lens, i.iso, i.f_number, i.exposure, i.focal_mm, \
    i.orientation, i.gps_lat, i.gps_lon, i.avg_color, i.checksum, i.content_hash, \
    i.thumb_recipe, i.thumb_error, i.opt_error";

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    // The catalogue is the app's own file, but it is also the one file a
    // disk error or an interrupted write can damage. Defensive mode refuses
    // writes that would corrupt the schema, and the limits keep a damaged
    // file from asking for unbounded memory or attaching other databases.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
    // No row in this catalogue is anywhere near 64 MiB (the largest are the
    // search vectors). The cap turns a corrupted length field into a read
    // error instead of a multi-gigabyte allocation.
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 64 * 1024 * 1024);
    // WAL lets a read (the UI) overlap a write (thumbnails/import). FULL
    // sync keeps the catalogue durable across a power loss, not just a
    // process crash.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 268435456;
         PRAGMA fullfsync = ON;
         PRAGMA secure_delete = ON;",
    )?;
    let journal: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") {
        return Err(crate::error::Error::Msg(
            "the catalogue could not use WAL; keep the library on a local disk, not a network share"
                .into(),
        ));
    }
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let app_id: i64 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(crate::error::Error::Msg(format!(
            "this library was written by a newer version of Forbidden Library (schema {version})"
        )));
    }
    if app_id != 0 && app_id != APPLICATION_ID {
        return Err(crate::error::Error::Msg(
            "library.db belongs to another application; Forbidden Library will not modify it"
                .into(),
        ));
    }
    if version == 0 {
        // A schema-less file that already has tables is somebody else's
        // database, however it got here. Never create Forbidden Library's tables
        // inside it.
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if tables != 0 || app_id != 0 {
            return Err(crate::error::Error::Msg(
                "library.db is not a Forbidden Library catalogue; refusing to modify it".into(),
            ));
        }
    }
    if version == 0 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE items (
                id TEXT PRIMARY KEY NOT NULL,
                kind TEXT NOT NULL,
                mime TEXT NOT NULL,
                ext TEXT NOT NULL,
                original_ext TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                original_bytes INTEGER NOT NULL,
                width INTEGER,
                height INTEGER,
                duration_ms INTEGER,
                captured_at INTEGER NOT NULL,
                captured_source TEXT NOT NULL,
                added_at INTEGER NOT NULL,
                modified_at INTEGER NOT NULL,
                original_name TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                thumb_state TEXT NOT NULL DEFAULT 'pending',
                opt_state TEXT NOT NULL DEFAULT 'pending',
                opt_kind TEXT,
                deleted_at INTEGER,
                camera_make TEXT,
                camera_model TEXT,
                lens TEXT,
                iso INTEGER,
                f_number REAL,
                exposure REAL,
                focal_mm REAL,
                orientation INTEGER NOT NULL DEFAULT 1,
                gps_lat REAL,
                gps_lon REAL,
                avg_color INTEGER NOT NULL DEFAULT 2960685,
                checksum TEXT NOT NULL,
                content_hash TEXT NOT NULL
             );
             CREATE INDEX items_timeline ON items(deleted_at, captured_at);
             CREATE INDEX items_content ON items(content_hash);
             CREATE INDEX items_path ON items(rel_path);
             CREATE TABLE sources (
                item_id TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                imported_at INTEGER NOT NULL,
                PRIMARY KEY (item_id, path)
             );
             CREATE TABLE albums (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE album_items (
                album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
                item_id TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                added_at INTEGER NOT NULL,
                PRIMARY KEY (album_id, item_id)
             );
             CREATE TABLE failures (
                path TEXT PRIMARY KEY NOT NULL,
                error TEXT NOT NULL,
                at INTEGER NOT NULL
             );
             CREATE TABLE meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    if version < 2 {
        // Semantic search: one unit-length vector per item, keyed by the
        // embedding model that produced it. A model change re-indexes in
        // place instead of mixing incompatible spaces.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS item_vectors (
                item_id TEXT PRIMARY KEY NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                dims INTEGER NOT NULL,
                vector BLOB NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS item_vectors_model ON item_vectors(model, dims);
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    if version < 3 {
        // The timeline is ordered by capture time, and the library opens on
        // that ordering every time. Without a matching index SQLite sorted
        // the whole catalogue on each read (`COALESCE` in the ORDER BY made
        // sure of it); with one, the newest page is a short reverse scan.
        conn.execute_batch(
            "BEGIN;
             DROP INDEX IF EXISTS items_timeline;
             CREATE INDEX IF NOT EXISTS items_timeline_captured
                ON items(deleted_at, captured_at DESC, added_at DESC, id DESC);
             CREATE INDEX IF NOT EXISTS items_active_added
                ON items(deleted_at, added_at DESC);
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
        conn.execute_batch("ANALYZE;")?;
    }
    if version < 4 {
        // Schema 4 adds what the interface and the workers needed next: the
        // preview recipe that produced a thumbnail, the reason a preview or
        // an optimization failed, an index for duplicate detection by size,
        // and the two pending-work indexes.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE items ADD COLUMN thumb_recipe INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE items ADD COLUMN thumb_error TEXT;
             ALTER TABLE items ADD COLUMN opt_error TEXT;
             CREATE INDEX IF NOT EXISTS items_bytes ON items(bytes);
             CREATE INDEX IF NOT EXISTS items_thumb_pending
                ON items(thumb_state) WHERE thumb_state = 'pending';
             CREATE INDEX IF NOT EXISTS items_opt_pending
                ON items(opt_state) WHERE opt_state = 'pending';
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
        conn.execute_batch("ANALYZE;")?;
    }
    if version < 5 {
        // Schema 5 optimizes images at import, so nothing scans for pending
        // rewrites any more. Duplicate detection by size also matches the
        // original's size, which is what a re-import presents, so it needs
        // its own index.
        conn.execute_batch(
            "BEGIN;
             CREATE INDEX IF NOT EXISTS items_original_bytes ON items(original_bytes);
             DROP INDEX IF EXISTS items_opt_pending;
             PRAGMA user_version = 5;
             COMMIT;",
        )?;
        conn.execute_batch("ANALYZE;")?;
    }
    if version < 6 {
        // Music: one row per audio item with its tags and stream details. The
        // album index keeps an album's tracks together when they are listed in
        // album order, which is how a music library is browsed.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS audio_tags (
                item_id TEXT PRIMARY KEY NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                title TEXT,
                artist TEXT,
                album TEXT,
                album_artist TEXT,
                track_no INTEGER,
                track_total INTEGER,
                disc_no INTEGER,
                year INTEGER,
                genre TEXT,
                codec TEXT NOT NULL,
                bitrate INTEGER,
                sample_rate INTEGER,
                channels INTEGER,
                has_cover INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS audio_tags_album
                ON audio_tags(album_artist, album, disc_no, track_no);
             PRAGMA user_version = 6;
             COMMIT;",
        )?;
    }
    if version < 7 {
        // Collections are the user's own lists, so their tables are named for
        // what they are. Albums made from music tags are not stored at all.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE albums RENAME TO collections;
             ALTER TABLE album_items RENAME TO collection_items;
             ALTER TABLE collection_items RENAME COLUMN album_id TO collection_id;
             PRAGMA user_version = 7;
             COMMIT;",
        )?;
    }
    if version < 8 {
        // Schema 8 drops the bookmark column that an earlier build added to
        // collections. Libraries that never had it only bump the version.
        let has_bookmarks: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('collections') WHERE name = 'bookmarked'",
            [],
            |row| row.get(0),
        )?;
        let drop = if has_bookmarks != 0 {
            "ALTER TABLE collections DROP COLUMN bookmarked;"
        } else {
            ""
        };
        conn.execute_batch(&format!(
            "BEGIN;
             {drop}
             PRAGMA user_version = 8;
             COMMIT;"
        ))?;
    }
    if version < 9 {
        // Synced lyrics, kept as the LRC text they came from. Only songs that
        // were imported with a matching `.lrc` have a row.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS lyrics (
                item_id TEXT PRIMARY KEY NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                lrc TEXT NOT NULL
             );
             PRAGMA user_version = 9;
             COMMIT;",
        )?;
    }
    if version < 10 {
        // Password locks. A locked collection keeps its key material here and
        // its sealed files in `vault/`; while it is unlocked its items carry
        // `vault_id` and stay out of every other page. `vault_purge` lists
        // plaintext files that must not outlive the state that made them
        // (see `locked::purge_pending`): it is written in the same transaction
        // as the change, so a crash can only leave a file that is still listed.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE collections ADD COLUMN vault_kdf TEXT;
             ALTER TABLE collections ADD COLUMN vault_salt BLOB;
             ALTER TABLE collections ADD COLUMN vault_wrap BLOB;
             ALTER TABLE collections ADD COLUMN vault_open INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE collections ADD COLUMN vault_seal TEXT;
             ALTER TABLE items ADD COLUMN vault_id INTEGER REFERENCES collections(id);
             CREATE INDEX IF NOT EXISTS items_vault ON items(vault_id) WHERE vault_id IS NOT NULL;
             CREATE TABLE IF NOT EXISTS vault_purge (
                path TEXT PRIMARY KEY NOT NULL,
                item_id TEXT NOT NULL
             );
             PRAGMA user_version = 10;
             COMMIT;",
        )?;
    }
    // Libraries written before schema 4 carry the right tables but no id.
    // Only write it when it is missing: rewriting the header on every open
    // would dirty the database for nothing.
    if app_id != APPLICATION_ID {
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    }
    Ok(())
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let lat: Option<f64> = row.get(28)?;
    let lon: Option<f64> = row.get(29)?;
    Ok(Item {
        id: row.get(0)?,
        kind: Kind::parse(&row.get::<_, String>(1)?),
        mime: row.get(2)?,
        ext: row.get(3)?,
        original_ext: row.get(4)?,
        bytes: row.get::<_, i64>(5)?.max(0) as u64,
        original_bytes: row.get::<_, i64>(6)?.max(0) as u64,
        width: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u32),
        height: row.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u32),
        duration_ms: row.get(9)?,
        captured_at: row.get(10)?,
        captured_source: row.get(11)?,
        added_at: row.get(12)?,
        modified_at: row.get(13)?,
        original_name: row.get(14)?,
        rel_path: row.get(15)?,
        thumb_state: ThumbState::parse(&row.get::<_, String>(16)?),
        opt_state: OptState::parse(&row.get::<_, String>(17)?),
        opt_kind: row.get(18)?,
        deleted_at: row.get(19)?,
        camera_make: row.get(20)?,
        camera_model: row.get(21)?,
        lens: row.get(22)?,
        iso: row.get(23)?,
        f_number: row.get(24)?,
        exposure: row.get(25)?,
        focal_mm: row.get(26)?,
        orientation: row.get::<_, i64>(27)? as i32,
        gps: match (lat, lon) {
            (Some(lat), Some(lon)) => Some((lat, lon)),
            _ => None,
        },
        avg_color: row.get::<_, i64>(30)?.max(0) as u32,
        checksum: row.get(31)?,
        content_hash: row.get(32)?,
        thumb_recipe: row.get(33)?,
        thumb_error: row.get(34)?,
        opt_error: row.get(35)?,
    })
}

/// The single-item read. Built once, so every call hands the connection the
/// same text and its prepared statement stays in the cache.
static GET_ITEM_SQL: LazyLock<String> =
    LazyLock::new(|| format!("SELECT {ITEM_COLUMNS} FROM items i WHERE i.id = ?1"));

pub fn get_item(conn: &Connection, id: &str) -> Result<Option<Item>> {
    let mut stmt = conn.prepare_cached(&GET_ITEM_SQL)?;
    Ok(stmt.query_row(params![id], row_to_item).optional()?)
}

pub(crate) fn item_vault_id(conn: &Connection, id: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached("SELECT vault_id FROM items WHERE id = ?1")?;
    Ok(stmt
        .query_row(params![id], |row| row.get::<_, Option<i64>>(0))
        .optional()?
        .flatten())
}

pub fn find_id_by_content_hash(conn: &Connection, hash: &str) -> Result<Option<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM items WHERE id = ?1 OR content_hash = ?1 LIMIT 1")?;
    Ok(stmt.query_row(params![hash], |row| row.get(0)).optional()?)
}

const INSERT_ITEM: &str = "
    INSERT INTO items (
        id, kind, mime, ext, original_ext, bytes, original_bytes,
        width, height, duration_ms, captured_at, captured_source,
        added_at, modified_at, original_name, rel_path, thumb_state,
        opt_state, opt_kind, deleted_at, camera_make,
        camera_model, lens, iso, f_number, exposure, focal_mm,
        orientation, gps_lat, gps_lon, avg_color, checksum, content_hash,
        thumb_recipe, opt_error
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
        ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
        ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35
    ) ON CONFLICT(id) DO NOTHING";

const INSERT_SOURCE: &str =
    "INSERT OR IGNORE INTO sources (item_id, path, imported_at) VALUES (?1, ?2, ?3)";

/// Insert a batch of catalogue rows in one transaction.
///
/// With `synchronous = FULL` every commit flushes the WAL, and a per-file
/// commit made that flush the largest single cost of importing a folder of
/// small files. One transaction for the batch keeps the durability contract
/// (the rows are on disk before an import is reported) and pays one flush for
/// the whole group.
///
/// The result says, per row, whether it was new. `false` means that content
/// is already catalogued, which the caller reports as a duplicate.
pub fn insert_items(
    conn: &Connection,
    rows: &[(&Item, &Path, Option<&AudioMeta>)],
) -> Result<Vec<bool>> {
    let tx = conn.unchecked_transaction()?;
    let now = time::now_ms();
    let mut inserted = Vec::with_capacity(rows.len());
    for (item, source, audio) in rows {
        let is_new = tx.execute(
            INSERT_ITEM,
            params![
                item.id,
                item.kind.as_str(),
                item.mime,
                item.ext,
                item.original_ext,
                item.bytes as i64,
                item.original_bytes as i64,
                item.width.map(|v| v as i64),
                item.height.map(|v| v as i64),
                item.duration_ms,
                item.captured_at,
                item.captured_source,
                item.added_at,
                item.modified_at,
                item.original_name,
                item.rel_path,
                item.thumb_state.as_str(),
                item.opt_state.as_str(),
                item.opt_kind,
                item.deleted_at,
                item.camera_make,
                item.camera_model,
                item.lens,
                item.iso,
                item.f_number,
                item.exposure,
                item.focal_mm,
                item.orientation as i64,
                item.gps.map(|g| g.0),
                item.gps.map(|g| g.1),
                item.avg_color as i64,
                item.checksum,
                item.content_hash,
                item.thumb_recipe,
                item.opt_error,
            ],
        )? > 0;
        tx.execute(
            INSERT_SOURCE,
            params![item.id, source.to_string_lossy(), now],
        )?;
        // A row that already existed keeps the tags it was first imported with.
        if let (true, Some(audio)) = (is_new, audio) {
            insert_audio(&tx, &item.id, audio)?;
        }
        inserted.push(is_new);
    }
    tx.commit()?;
    Ok(inserted)
}

const INSERT_AUDIO: &str = "
    INSERT INTO audio_tags (
        item_id, title, artist, album, album_artist, track_no, track_total,
        disc_no, year, genre, codec, bitrate, sample_rate, channels, has_cover
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15
    ) ON CONFLICT(item_id) DO NOTHING";

fn insert_audio(conn: &Connection, id: &str, audio: &AudioMeta) -> Result<()> {
    let tags = &audio.tags;
    conn.execute(
        INSERT_AUDIO,
        params![
            id,
            tags.title,
            tags.artist,
            tags.album,
            tags.album_artist,
            tags.track_no,
            tags.track_total,
            tags.disc_no,
            tags.year,
            tags.genre,
            audio.codec,
            audio.bitrate,
            audio.sample_rate,
            audio.channels,
            audio.has_cover,
        ],
    )?;
    Ok(())
}

pub fn insert_item(
    conn: &Connection,
    item: &Item,
    source: &Path,
    audio: Option<&AudioMeta>,
) -> Result<bool> {
    Ok(insert_items(conn, &[(item, source, audio)])?.remove(0))
}

/// Attach LRC text to a song. A song keeps the lyrics it was first imported
/// with, so this does nothing when it already has some.
pub fn set_lyrics(conn: &Connection, id: &str, lrc: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO lyrics (item_id, lrc) VALUES (?1, ?2) ON CONFLICT(item_id) DO NOTHING",
        params![id, lrc],
    )?;
    Ok(())
}

/// The LRC text attached to a song, if any.
pub fn lyrics(conn: &Connection, id: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT lrc FROM lyrics WHERE item_id = ?1")?;
    Ok(stmt.query_row(params![id], |row| row.get(0)).optional()?)
}

/// The music details of one item, or `None` when it is not a music file.
/// `duration_ms` is the item's own length, so it is read from the item row.
pub fn audio_meta(conn: &Connection, id: &str) -> Result<Option<AudioMeta>> {
    let mut stmt = conn.prepare_cached(
        "SELECT a.title, a.artist, a.album, a.album_artist, a.track_no,
                    a.track_total, a.disc_no, a.year, a.genre, a.codec, a.bitrate,
                    a.sample_rate, a.channels, a.has_cover, i.duration_ms
             FROM audio_tags a JOIN items i ON i.id = a.item_id
             WHERE a.item_id = ?1",
    )?;
    Ok(stmt
        .query_row(params![id], |row| {
            let duration_ms: Option<i64> = row.get(14)?;
            audio_from_row(row, 0, duration_ms)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
        })
        .optional()?)
}

/// Read the tag columns that start at `at`, in the order `audio_tags` declares
/// them after the id. `None` when the row has no tags (a LEFT JOIN miss).
fn audio_from_row(
    row: &rusqlite::Row<'_>,
    at: usize,
    duration_ms: Option<i64>,
) -> rusqlite::Result<Option<AudioMeta>> {
    let Some(codec) = row.get::<_, Option<String>>(at + 9)? else {
        return Ok(None);
    };
    Ok(Some(AudioMeta {
        tags: AudioTags {
            title: row.get(at)?,
            artist: row.get(at + 1)?,
            album: row.get(at + 2)?,
            album_artist: row.get(at + 3)?,
            track_no: row.get(at + 4)?,
            track_total: row.get(at + 5)?,
            disc_no: row.get(at + 6)?,
            year: row.get(at + 7)?,
            genre: row.get(at + 8)?,
        },
        codec,
        bitrate: row.get(at + 10)?,
        sample_rate: row.get(at + 11)?,
        channels: row.get(at + 12)?,
        has_cover: row.get(at + 13)?,
        duration_ms,
    }))
}

/// Every music item in album order, each with its tags: album artist (or the
/// artist) first, then album, then disc and track. Music with no album comes
/// last. The album's tracks therefore sit together and in playing order.
pub fn music_library(conn: &Connection) -> Result<Vec<(Item, Option<AudioMeta>)>> {
    let sql = format!(
        "SELECT {ITEM_COLUMNS},
                a.title, a.artist, a.album, a.album_artist, a.track_no,
                a.track_total, a.disc_no, a.year, a.genre, a.codec, a.bitrate,
                a.sample_rate, a.channels, a.has_cover
         FROM items i
         LEFT JOIN audio_tags a ON a.item_id = i.id
         WHERE i.kind = 'audio' AND i.deleted_at IS NULL AND i.vault_id IS NULL
         ORDER BY (a.album IS NULL OR a.album = ''),
                  LOWER(COALESCE(NULLIF(a.album_artist, ''), a.artist, '')),
                  LOWER(COALESCE(a.album, '')),
                  COALESCE(a.disc_no, 0),
                  COALESCE(a.track_no, 0),
                  LOWER(COALESCE(a.title, i.original_name)),
                  i.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let item = row_to_item(row)?;
        // The item's columns come first, so the tag columns start after them.
        let audio = audio_from_row(row, 36, item.duration_ms)?;
        Ok((item, audio))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn add_source(conn: &Connection, id: &str, source: &Path) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sources (item_id, path, imported_at) VALUES (?1, ?2, ?3)",
        params![id, source.to_string_lossy(), time::now_ms()],
    )?;
    Ok(())
}

/// The filter every timeline read shares, as a `WHERE` clause plus its bound
/// arguments. Kept in one place so paging, counting and id-only reads can
/// never drift apart.
fn filter_sql(filter: &TimelineFilter) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if filter.trashed {
        sql.push_str(" AND i.deleted_at IS NOT NULL");
    } else {
        sql.push_str(" AND i.deleted_at IS NULL");
    }
    if let Some(kind) = filter.kind {
        sql.push_str(" AND i.kind = ?");
        args.push(Box::new(kind.as_str().to_string()));
    }
    if filter.none {
        sql.push_str(" AND 0 = 1");
    }
    if let Some(collection) = filter.collection_id {
        // A locked collection's items are visible only inside it, so the
        // collection filter is the one place they are allowed through.
        sql.push_str(" AND i.id IN (SELECT item_id FROM collection_items WHERE collection_id = ?)");
        args.push(Box::new(collection));
    } else {
        sql.push_str(" AND i.vault_id IS NULL");
    }
    (sql, args)
}

/// The trash reads by when things were thrown away: the newest deletion is
/// first, the oldest (closest to expiring) last. Everything else reads the
/// capture timeline; `captured_at` is NOT NULL and always set by the
/// importer, so a coalesce there would only cost the index.
fn order_sql(filter: &TimelineFilter) -> &'static str {
    if filter.trashed {
        " ORDER BY i.deleted_at DESC, i.id DESC"
    } else {
        " ORDER BY i.captured_at DESC, i.added_at DESC, i.id DESC"
    }
}

pub fn list_items(conn: &Connection, filter: &TimelineFilter) -> Result<Vec<Item>> {
    let (where_sql, mut args) = filter_sql(filter);
    let mut sql = format!("SELECT {ITEM_COLUMNS} FROM items i{where_sql}");
    sql.push_str(order_sql(filter));
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        args.push(Box::new(limit as i64));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(args.iter().map(|value| value.as_ref())),
        row_to_item,
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Where the previous page stopped. Keyset paging keeps every page a short
/// index scan: no offset, no sorting, and a row inserted on another thread
/// cannot make an item appear twice or disappear between pages.
#[derive(Clone, Debug)]
pub struct PageCursor {
    pub captured_at: i64,
    pub added_at: i64,
    pub deleted_at: Option<i64>,
    pub id: String,
}

impl PageCursor {
    pub fn after(item: &Item) -> Self {
        Self {
            captured_at: item.captured_at,
            added_at: item.added_at,
            deleted_at: item.deleted_at,
            id: item.id.clone(),
        }
    }
}

/// One bounded page of the timeline plus the totals the scrollbar needs.
#[derive(Clone, Debug, Default)]
pub struct ItemPage {
    pub items: Vec<Item>,
    /// The matching total. Counted on the first page only, so later pages
    /// carry `None` and the caller keeps the total it already has.
    pub total: Option<usize>,
    pub cursor: Option<PageCursor>,
    /// True when this page reached the end of the filtered catalogue.
    pub done: bool,
}

pub fn count_items(conn: &Connection, filter: &TimelineFilter) -> Result<usize> {
    let (where_sql, args) = filter_sql(filter);
    let sql = format!("SELECT COUNT(*) FROM items i{where_sql}");
    let count: i64 = conn.query_row(
        &sql,
        params_from_iter(args.iter().map(|value| value.as_ref())),
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

pub fn item_page(
    conn: &Connection,
    filter: &TimelineFilter,
    after: Option<&PageCursor>,
    limit: usize,
) -> Result<ItemPage> {
    let limit = limit.clamp(1, 5000);
    let total = match after {
        None => Some(count_items(conn, filter)?),
        Some(_) => None,
    };
    let (where_sql, mut args) = filter_sql(filter);
    let mut sql = format!("SELECT {ITEM_COLUMNS} FROM items i{where_sql}");
    if let Some(cursor) = after {
        if filter.trashed {
            sql.push_str(" AND (i.deleted_at < ? OR (i.deleted_at = ? AND i.id < ?))");
            let at = cursor.deleted_at.unwrap_or(0);
            args.push(Box::new(at));
            args.push(Box::new(at));
            args.push(Box::new(cursor.id.clone()));
        } else {
            sql.push_str(
                " AND (i.captured_at < ? OR (i.captured_at = ? AND \
                 (i.added_at < ? OR (i.added_at = ? AND i.id < ?))))",
            );
            args.push(Box::new(cursor.captured_at));
            args.push(Box::new(cursor.captured_at));
            args.push(Box::new(cursor.added_at));
            args.push(Box::new(cursor.added_at));
            args.push(Box::new(cursor.id.clone()));
        }
    }
    sql.push_str(order_sql(filter));
    // One extra row tells us whether another page exists without a second
    // count query.
    sql.push_str(" LIMIT ?");
    args.push(Box::new(limit as i64 + 1));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(args.iter().map(|value| value.as_ref())),
        row_to_item,
    )?;
    let mut items: Vec<Item> = rows.collect::<std::result::Result<_, _>>()?;
    let more = items.len() > limit;
    if more {
        items.truncate(limit);
    }
    let cursor = items.last().map(PageCursor::after);
    Ok(ItemPage {
        items,
        total,
        cursor,
        done: !more,
    })
}

/// Every id in the filtered timeline, for select-all. Ids only: no rows are
/// decoded and nothing is cloned into the interface.
pub fn item_ids(conn: &Connection, filter: &TimelineFilter) -> Result<Vec<String>> {
    let (where_sql, args) = filter_sql(filter);
    let mut sql = format!("SELECT i.id FROM items i{where_sql}");
    sql.push_str(order_sql(filter));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(args.iter().map(|value| value.as_ref())),
        |row| row.get::<_, String>(0),
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// `(id, captured_at)` for the filtered timeline. Date searches and collection
/// matching need to compare dates against every item without materializing a
/// full `Item` for each one; this is the narrowest read that can answer them.
pub fn capture_index(conn: &Connection, filter: &TimelineFilter) -> Result<Vec<(String, i64)>> {
    let (where_sql, args) = filter_sql(filter);
    let mut sql = format!("SELECT i.id, i.captured_at FROM items i{where_sql}");
    sql.push_str(order_sql(filter));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(args.iter().map(|value| value.as_ref())),
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Whether any catalogue row has this byte length. A re-import only needs to
/// hash its source when the answer is yes; a brand-new file skips the read.
pub fn has_items_of_size(conn: &Connection, bytes: u64) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM items WHERE bytes = ?1 OR original_bytes = ?1)",
    )?;
    Ok(stmt.query_row(params![bytes as i64], |row| row.get(0))?)
}

/// Encode a unit vector as little-endian f32s. SQLite stores it as a blob;
/// the layout is private to this module.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8], dims: usize) -> Option<Vec<f32>> {
    if bytes.len() != dims * 4 {
        return None;
    }
    let mut vector = Vec::with_capacity(dims);
    for chunk in bytes.chunks_exact(4) {
        vector.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    vector
        .iter()
        .all(|value| value.is_finite())
        .then_some(vector)
}

/// Store a batch in one transaction: indexing thousands of photos should not
/// cost one fsync per photo.
pub fn set_embeddings(conn: &Connection, model: &str, rows: &[(String, Vec<f32>)]) -> Result<()> {
    let rows: Vec<&(String, Vec<f32>)> = rows
        .iter()
        .filter(|(_, vector)| {
            !vector.is_empty() && vector.len() <= 4096 && vector.iter().all(|v| v.is_finite())
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    let now = time::now_ms();
    {
        let mut stmt = tx.prepare(
            "INSERT INTO item_vectors (item_id, model, dims, vector, created_at)
             SELECT id, ?2, ?3, ?4, ?5 FROM items
             WHERE id = ?1 AND vault_id IS NULL AND deleted_at IS NULL
             ON CONFLICT(item_id) DO UPDATE SET
                model = excluded.model,
                dims = excluded.dims,
                vector = excluded.vector,
                created_at = excluded.created_at",
        )?;
        for (id, vector) in rows {
            stmt.execute(params![
                id,
                model,
                vector.len() as i64,
                encode_vector(vector),
                now
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Every vector for `model`, in catalogue order, laid out exactly as the live
/// index wants it: one contiguous buffer and a parallel list of ids. Decoding
/// straight into that buffer avoids an allocation per photo and the copy the
/// row-by-row path would add on a large library.
pub fn flat_embeddings(
    conn: &Connection,
    model: &str,
    dims: usize,
) -> Result<(Vec<String>, Vec<f32>)> {
    if dims == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let expected = dims as i64;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM item_vectors WHERE model = ?1 AND dims = ?2",
        params![model, expected],
        |row| row.get(0),
    )?;
    let mut ids = Vec::with_capacity(count.max(0) as usize);
    let mut vectors = Vec::with_capacity(count.max(0) as usize * dims);
    let mut stmt = conn.prepare(
        "SELECT item_id, vector FROM item_vectors
         WHERE model = ?1 AND dims = ?2 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![model, expected], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (id, bytes) = row?;
        if bytes.len() != dims * 4 {
            continue;
        }
        let start = vectors.len();
        for chunk in bytes.chunks_exact(4) {
            let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if !value.is_finite() {
                vectors.truncate(start);
                break;
            }
            vectors.push(value);
        }
        if vectors.len() == start + dims {
            ids.push(id);
        }
    }
    Ok((ids, vectors))
}

/// Every vector for `model`, in catalogue order. Corrupt or foreign rows are
/// skipped rather than failing the whole library.
pub fn embeddings(conn: &Connection, model: &str) -> Result<Vec<(String, Vec<f32>)>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, dims, vector FROM item_vectors WHERE model = ?1 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![model], |row| {
        let id: String = row.get(0)?;
        let dims: i64 = row.get(1)?;
        let bytes: Vec<u8> = row.get(2)?;
        Ok((id, dims, bytes))
    })?;
    let mut vectors = Vec::new();
    for row in rows {
        let (id, dims, bytes) = row?;
        if dims <= 0 || dims > 4096 {
            continue;
        }
        if let Some(vector) = decode_vector(&bytes, dims as usize) {
            vectors.push((id, vector));
        }
    }
    Ok(vectors)
}

/// Media that has no vector for this model yet, newest first. Trashed items
/// keep their vectors but are not re-indexed.
pub fn pending_embeddings(conn: &Connection, model: &str, limit: usize) -> Result<Vec<Item>> {
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM items i
         LEFT JOIN item_vectors v ON v.item_id = i.id AND v.model = ?1
         WHERE v.item_id IS NULL
           AND i.deleted_at IS NULL
           AND i.vault_id IS NULL
           AND i.kind IN ('image', 'video')
         ORDER BY i.added_at DESC, i.id DESC
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![model, limit as i64], row_to_item)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn embedding_count(conn: &Connection, model: &str) -> Result<usize> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM item_vectors WHERE model = ?1",
        params![model],
        |row| row.get::<_, i64>(0),
    )? as usize)
}

pub fn set_thumb_state(
    conn: &Connection,
    id: &str,
    state: ThumbState,
    avg_color: Option<u32>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE items SET thumb_state = ?2, thumb_recipe = ?3, thumb_error = ?4, \
         avg_color = COALESCE(?5, avg_color) WHERE id = ?1",
        params![
            id,
            state.as_str(),
            crate::thumb::RECIPE,
            error,
            avg_color.map(|color| color as i64)
        ],
    )?;
    Ok(())
}

pub fn path_references(conn: &Connection, rel_path: &str) -> Result<usize> {
    let mut stmt = conn.prepare_cached("SELECT COUNT(*) FROM items WHERE rel_path = ?1")?;
    Ok(stmt.query_row(params![rel_path], |row| row.get::<_, i64>(0))? as usize)
}

pub fn trash(conn: &Connection, ids: &[String], at: i64) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let mut changed = 0;
    {
        let mut stmt =
            tx.prepare(
            "UPDATE items SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL AND vault_id IS NULL",
        )?;
        for id in ids {
            changed += stmt.execute(params![at, id])?;
        }
    }
    tx.commit()?;
    Ok(changed)
}

pub fn restore(conn: &Connection, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let mut changed = 0;
    {
        let mut stmt = tx.prepare("UPDATE items SET deleted_at = NULL WHERE id = ?1")?;
        for id in ids {
            changed += stmt.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// Delete many item rows in one transaction. Each separate statement would
/// commit, and with `synchronous = FULL` every commit waits for a disk flush.
pub fn delete_items(conn: &Connection, ids: &[&str]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached("DELETE FROM items WHERE id = ?1")?;
        for id in ids {
            stmt.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The lock a collection carries in the catalogue. `Unlocked` here means its
/// rows are present; whether this session holds its key is the caller's to
/// add, since the catalogue cannot know that.
pub fn collection_lock_state(conn: &Connection, id: i64) -> Result<Option<(bool, bool)>> {
    // `(is_locked, rows_present)`, or `None` when no such collection exists.
    Ok(conn
        .query_row(
            "SELECT vault_kdf IS NOT NULL, vault_open FROM collections WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?)
}

pub fn collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at,
            (SELECT COUNT(*) FROM collection_items ai JOIN items i ON i.id = ai.item_id
             WHERE ai.collection_id = a.id AND i.deleted_at IS NULL),
            a.vault_kdf IS NOT NULL, a.vault_open
         FROM collections a ORDER BY a.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |row| {
        let locked: bool = row.get(4)?;
        let open: bool = row.get(5)?;
        Ok(Collection {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            count: row.get(3)?,
            cover_item_ids: Vec::new(),
            lock: match (locked, open) {
                (false, _) => LockState::None,
                (true, false) => LockState::Locked,
                (true, true) => LockState::Unlocked,
            },
        })
    })?;
    let mut collections = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    // The collection's primary key makes each lookup cheap, so one query per collection
    // is fine and keeps the newest-first order without a grouped sort.
    let mut covers = conn.prepare(
        "SELECT ai.item_id FROM collection_items ai JOIN items i ON i.id = ai.item_id
         WHERE ai.collection_id = ?1 AND i.deleted_at IS NULL
         ORDER BY i.captured_at DESC LIMIT ?2",
    )?;
    for collection in &mut collections {
        collection.cover_item_ids = covers
            .query_map(
                params![collection.id, crate::model::COVER_LAYERS as i64],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<Vec<String>, _>>()?;
    }
    Ok(collections)
}

pub fn create_collection(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::error::Error::Msg(
            "collection name cannot be empty".into(),
        ));
    }
    conn.execute(
        "INSERT INTO collections (name, created_at) VALUES (?1, ?2)
         ON CONFLICT(name) DO NOTHING",
        params![name, time::now_ms()],
    )?;
    let id = conn.query_row(
        "SELECT id FROM collections WHERE name = ?1 COLLATE NOCASE",
        params![name],
        |row| row.get(0),
    )?;
    Ok(id)
}

pub fn delete_collection(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM collections WHERE id = ?1", params![id])?;
    Ok(())
}
/// Add items to an ordinary collection. An item inside a locked collection is
/// skipped: it can only leave that collection by being released from it.
pub fn add_to_collection(conn: &Connection, collection_id: i64, ids: &[String]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut added = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO collection_items (collection_id, item_id, added_at)
             SELECT ?1, i.id, ?3 FROM items i WHERE i.id = ?2 AND i.vault_id IS NULL",
        )?;
        for id in ids {
            added += stmt.execute(params![collection_id, id, time::now_ms()])?;
        }
    }
    tx.commit()?;
    Ok(added)
}

/// Put items into a locked collection, which takes them out of every other
/// collection and off every page. Trashed items are not taken, and an item
/// already inside a locked collection stays where it is.
pub fn add_to_locked(conn: &Connection, collection_id: i64, ids: &[String]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let now = time::now_ms();
    let mut added = 0;
    {
        let mut mark = tx.prepare(
            "UPDATE items SET vault_id = ?1
             WHERE id = ?2 AND vault_id IS NULL AND deleted_at IS NULL",
        )?;
        let mut drop_others = tx.prepare("DELETE FROM collection_items WHERE item_id = ?1")?;
        let mut join = tx.prepare(
            "INSERT INTO collection_items (collection_id, item_id, added_at) VALUES (?1, ?2, ?3)",
        )?;
        let mut forget_vector = tx.prepare("DELETE FROM item_vectors WHERE item_id = ?1")?;
        for id in ids {
            if mark.execute(params![collection_id, id])? == 0 {
                continue;
            }
            drop_others.execute(params![id])?;
            join.execute(params![collection_id, id, now])?;
            // Locked items are not searchable, so their vectors are dropped
            // rather than kept where a search could still rank them.
            forget_vector.execute(params![id])?;
            added += 1;
        }
    }
    tx.commit()?;
    Ok(added)
}

/// Take items out of a collection. The items themselves are untouched, except
/// that an item leaving a locked collection returns to the library.
pub fn remove_from_collection(
    conn: &Connection,
    collection_id: i64,
    ids: &[String],
) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let mut removed = 0;
    {
        let mut stmt =
            tx.prepare("DELETE FROM collection_items WHERE collection_id = ?1 AND item_id = ?2")?;
        let mut release =
            tx.prepare("UPDATE items SET vault_id = NULL WHERE id = ?2 AND vault_id = ?1")?;
        for id in ids {
            removed += stmt.execute(params![collection_id, id])?;
            release.execute(params![collection_id, id])?;
        }
    }
    tx.commit()?;
    Ok(removed)
}

/// Return every item of a locked collection to the library and remove its
/// lock. The items' files are already plaintext in the store while the
/// collection is unlocked, so nothing else has to change.
pub fn release_locked(conn: &Connection, collection_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let released = tx.execute(
        "UPDATE items SET vault_id = NULL WHERE vault_id = ?1",
        params![collection_id],
    )?;
    tx.execute(
        "UPDATE collections SET vault_kdf = NULL, vault_salt = NULL, vault_wrap = NULL,
            vault_open = 0, vault_seal = NULL
         WHERE id = ?1",
        params![collection_id],
    )?;
    tx.commit()?;
    Ok(released)
}

/// Ids of the items among `ids` that the library shows. Anything inside a
/// locked collection is dropped, so a search that remembers an item from
/// before it was locked cannot surface it.
pub fn visible_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let mut visible = Vec::with_capacity(ids.len());
    let mut stmt = conn.prepare("SELECT id FROM items WHERE id = ?1 AND vault_id IS NULL")?;
    for id in ids {
        if let Some(found) = stmt
            .query_row(params![id], |row| row.get::<_, String>(0))
            .optional()?
        {
            visible.push(found);
        }
    }
    Ok(visible)
}

/// Every collection that holds at least one of `ids`.
pub fn collection_ids_for_items(conn: &Connection, ids: &[String]) -> Result<Vec<i64>> {
    let mut collections = Vec::new();
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT DISTINCT collection_id FROM collection_items WHERE item_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| row.get::<_, i64>(0))?;
        for row in rows {
            collections.push(row?);
        }
    }
    collections.sort_unstable();
    collections.dedup();
    Ok(collections)
}

/// Collections that already hold every item in `ids`. The sheet uses this to
/// leave them out rather than offering an add that would do nothing.
pub fn collections_containing_all(conn: &Connection, ids: &[String]) -> Result<Vec<i64>> {
    let mut unique: Vec<&String> = ids.iter().collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return Ok(Vec::new());
    }
    let mut counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for chunk in unique.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT collection_id, COUNT(DISTINCT item_id) FROM collection_items
             WHERE item_id IN ({placeholders}) GROUP BY collection_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter().copied()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (collection_id, count) = row?;
            *counts.entry(collection_id).or_default() += count.max(0) as usize;
        }
    }
    let mut collections: Vec<i64> = counts
        .into_iter()
        .filter_map(|(collection_id, count)| (count == unique.len()).then_some(collection_id))
        .collect();
    collections.sort_unstable();
    Ok(collections)
}

pub fn record_failure(conn: &Connection, path: &Path, error: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO failures (path, error, at) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET error = excluded.error, at = excluded.at",
        params![path.to_string_lossy(), error, time::now_ms()],
    )?;
    Ok(())
}

/// Catalogue totals, entirely from SQL. This is what the interface asks for
/// on every load, so it must never touch the filesystem.
pub fn stats(conn: &Connection) -> Result<Stats> {
    let mut stats = Stats::default();
    {
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(bytes), 0), COALESCE(SUM(original_bytes), 0)
             FROM items WHERE deleted_at IS NULL AND vault_id IS NULL GROUP BY kind",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (kind, count, bytes, original) = row?;
            match kind.as_str() {
                "image" => stats.photos = count as usize,
                "video" => stats.videos = count as usize,
                "audio" => stats.audio = count as usize,
                _ => stats.other = count as usize,
            }
            stats.bytes += bytes.max(0) as u64;
            stats.original_bytes += original.max(0) as u64;
        }
    }
    stats.trash_items = conn.query_row(
        "SELECT COUNT(*) FROM items WHERE deleted_at IS NOT NULL AND vault_id IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize;
    stats.trash_bytes = conn.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM items WHERE deleted_at IS NOT NULL AND vault_id IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )? as u64;
    stats.pending_thumbs = conn.query_row(
        "SELECT COUNT(*) FROM items WHERE deleted_at IS NULL AND vault_id IS NULL AND thumb_state = 'pending'",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize;
    stats.optimized_items = conn.query_row(
        "SELECT COUNT(*) FROM items WHERE opt_state = 'optimized' AND vault_id IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize;
    Ok(stats)
}

/// Totals plus the filesystem checks that surface a missing store file or
/// preview. One stat per item: fast for a small library, seconds for a big
/// one, so only the verifier and Utilities ask for it.
pub fn stats_with_files(conn: &Connection, paths: &Paths) -> Result<Stats> {
    let mut stats = stats(conn)?;
    let mut stmt = conn.prepare("SELECT rel_path, thumb_state, id FROM items")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (rel, thumb, id) = row?;
        if !paths.abs(&rel).is_file() {
            stats.missing_files += 1;
        }
        if thumb == "ready" && !paths.thumb_path(&id).is_file() {
            stats.missing_thumbs += 1;
        }
    }
    Ok(stats)
}

/// Fetch several catalogue rows in one round trip, for album covers and any
/// other place that would otherwise query once per id.
pub fn items_by_ids(conn: &Connection, ids: &[String]) -> Result<Vec<Item>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::with_capacity(ids.len());
    // Stay well below SQLite's bound-parameter limit.
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT {ITEM_COLUMNS} FROM items i WHERE i.id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), row_to_item)?;
        for row in rows {
            items.push(row?);
        }
    }
    Ok(items)
}
/// SQLite's own structural check. Fast enough to run on demand.
pub fn database_check(conn: &Connection) -> bool {
    conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map(|answer| answer == "ok")
        .unwrap_or(false)
}

/// Minimal rows for the integrity walk.
pub struct VerifyRow {
    pub id: String,
    pub rel_path: String,
    pub checksum: String,
    pub thumb_state: String,
}

pub fn verify_rows(conn: &Connection) -> Result<Vec<VerifyRow>> {
    let mut stmt = conn.prepare("SELECT id, rel_path, checksum, thumb_state FROM items")?;
    let rows = stmt.query_map([], |row| {
        Ok(VerifyRow {
            id: row.get(0)?,
            rel_path: row.get(1)?,
            checksum: row.get(2)?,
            thumb_state: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first page of the timeline must come from the index. If a reader
    /// ever reintroduces a computed sort key, SQLite falls back to a full
    /// scan plus `USE TEMP B-TREE FOR ORDER BY` and every reload of a large
    /// library pays for sorting the whole catalogue.
    #[test]
    fn the_media_timeline_is_read_from_the_index() {
        let dir = std::env::temp_dir().join(format!(
            "forbiddenlibrary-plan-{}-{}",
            std::process::id(),
            crate::time::stamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open(&dir.join("library.db")).unwrap();
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM items i
                 WHERE i.deleted_at IS NULL
                 ORDER BY i.captured_at DESC, i.added_at DESC, i.id DESC
                 LIMIT 100",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("items_timeline_captured"),
            "timeline read did not use its index: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "timeline read sorted the catalogue: {plan}"
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_catalogue_is_marked_as_a_forbidden_library_database() {
        let dir = std::env::temp_dir().join(format!(
            "forbiddenlibrary-appid-{}-{}",
            std::process::id(),
            crate::time::stamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open(&dir.join("library.db")).unwrap();
        let app_id: i64 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(app_id, APPLICATION_ID);
        assert_eq!(version, SCHEMA_VERSION);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_database_is_never_migrated() {
        let dir = std::env::temp_dir().join(format!(
            "forbiddenlibrary-foreign-{}-{}",
            std::process::id(),
            crate::time::stamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('hello');")
            .unwrap();
        drop(conn);
        let error = open(&path).expect_err("a foreign file must be refused");
        assert!(
            error
                .to_string()
                .contains("not a Forbidden Library catalogue"),
            "unexpected error: {error}"
        );
        let conn = Connection::open(&path).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the foreign table must be left alone");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_schema_three_library_migrates_forward() {
        let dir = std::env::temp_dir().join(format!(
            "forbiddenlibrary-migrate-{}-{}",
            std::process::id(),
            crate::time::stamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        drop(open(&path).unwrap());
        {
            // Recreate the shape schema 3 had: no recipe or error columns and
            // none of schema 4's indexes.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS items_vault;
                 ALTER TABLE items DROP COLUMN vault_id;
                 ALTER TABLE collections DROP COLUMN vault_seal;
                 ALTER TABLE collections DROP COLUMN vault_open;
                 ALTER TABLE collections DROP COLUMN vault_wrap;
                 ALTER TABLE collections DROP COLUMN vault_salt;
                 ALTER TABLE collections DROP COLUMN vault_kdf;
                 DROP TABLE IF EXISTS vault_purge;
                 DROP INDEX IF EXISTS items_bytes;
                 DROP INDEX IF EXISTS items_thumb_pending;
                 DROP INDEX IF EXISTS items_opt_pending;
                 ALTER TABLE items DROP COLUMN thumb_recipe;
                 ALTER TABLE items DROP COLUMN thumb_error;
                 ALTER TABLE items DROP COLUMN opt_error;
                 ALTER TABLE collection_items RENAME COLUMN collection_id TO album_id;
                 ALTER TABLE collection_items RENAME TO album_items;
                 ALTER TABLE collections RENAME TO albums;
                 PRAGMA application_id = 0;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let conn = open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let columns: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('items')
                 WHERE name IN ('thumb_recipe', 'thumb_error', 'opt_error')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 3, "the migration must add the new columns");
        let app_id: i64 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(app_id, APPLICATION_ID);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
