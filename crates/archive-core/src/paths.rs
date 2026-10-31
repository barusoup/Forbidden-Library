use crate::error::Result;
use std::path::{Path, PathBuf};

/// Every directory the archive owns. The root is the library folder; all
/// paths are derived so a library can move as one unit.
#[derive(Clone, Debug)]
pub struct Paths {
    root: PathBuf,
    db: PathBuf,
    store: PathBuf,
    thumbs: PathBuf,
    tmp: PathBuf,
    logs: PathBuf,
    vault: PathBuf,
}

impl Paths {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let paths = Self {
            db: root.join("library.db"),
            store: root.join("store"),
            thumbs: root.join("thumbs"),
            tmp: root.join("tmp"),
            logs: root.join("logs"),
            vault: root.join("vault"),
            root,
        };
        for dir in [
            &paths.store,
            &paths.thumbs,
            &paths.tmp,
            &paths.logs,
            &paths.vault,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        for relative in ["store", "thumbs", "tmp", "logs", "vault"] {
            // logs is not used for catalogue file operations.
            if relative != "logs" {
                paths.checked_owned_path(relative)?;
            } else if is_link(&std::fs::symlink_metadata(paths.logs())?) {
                return Err(crate::error::Error::msg(
                    "archive logs crosses a reparse point",
                ));
            }
        }
        Ok(paths)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> &Path {
        &self.db
    }

    pub fn store(&self) -> &Path {
        &self.store
    }

    pub fn thumbs(&self) -> &Path {
        &self.thumbs
    }

    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    pub fn logs(&self) -> &Path {
        &self.logs
    }

    /// Where locked collections keep their sealed files: one folder per
    /// collection, holding one folder per sealing.
    pub fn vault(&self) -> &Path {
        &self.vault
    }

    pub fn vault_dir(&self, collection_id: i64) -> PathBuf {
        self.vault.join(collection_id.to_string())
    }

    /// Absolute path for a catalogue-relative path (`store/..`).
    pub fn abs(&self, relative: &str) -> PathBuf {
        self.root
            .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    /// Resolve an owned path while refusing escapes and existing reparse points.
    /// This checks a stable tree; it does not defend against a hostile process
    /// replacing components concurrently with filesystem operations.
    pub(crate) fn checked_owned_path(&self, relative: &str) -> Result<PathBuf> {
        let damaged = || crate::error::Error::msg("unsafe archive path");
        let normalized = relative.replace('\\', "/");
        let mut parts = normalized.split('/');
        if !matches!(parts.next(), Some("store" | "thumbs" | "vault" | "tmp")) {
            return Err(damaged());
        }
        if normalized.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.ends_with(['.', ' '])
                || part.chars().any(|ch| ch < ' ' || "<>:\"|?*".contains(ch))
        }) {
            return Err(damaged());
        }
        let mut path = self.root.clone();
        for part in normalized.split('/') {
            path.push(part);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => {
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::MetadataExt;
                        if meta.file_attributes() & 0x400 != 0 {
                            return Err(damaged());
                        }
                    }
                    if meta.file_type().is_symlink() {
                        return Err(damaged());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(path)
    }

    /// Content-addressed store path, split by the first two bytes of the
    /// hash so one folder never holds a whole library.
    pub fn store_rel(hash: &str, ext: &str) -> String {
        let hash = safe_id(hash);
        let ext = sanitize_ext(ext);
        let (a, b) = split_hash(&hash);
        format!("store/{a}/{b}/{hash}.{ext}")
    }

    pub fn thumb_path(&self, id: &str) -> PathBuf {
        let id = safe_id(id);
        let (a, _) = split_hash(&id);
        self.thumbs.join(a).join(format!("{id}_384.jpg"))
    }

    pub fn viewer_jpeg_path(&self, id: &str) -> PathBuf {
        let id = safe_id(id);
        let (a, _) = split_hash(&id);
        self.thumbs.join(a).join(format!("{id}_2048.jpg"))
    }

    pub fn viewer_png_path(&self, id: &str) -> PathBuf {
        let id = safe_id(id);
        let (a, _) = split_hash(&id);
        self.thumbs.join(a).join(format!("{id}_2048.png"))
    }

    /// Every preview file an item can have, relative to the library root and
    /// written with forward slashes, the way catalogue paths are.
    pub fn preview_rel_paths(id: &str) -> [String; 3] {
        let id = safe_id(id);
        let (a, _) = split_hash(&id);
        [
            format!("thumbs/{a}/{id}_384.jpg"),
            format!("thumbs/{a}/{id}_2048.jpg"),
            format!("thumbs/{a}/{id}_2048.png"),
        ]
    }
}

fn safe_id(id: &str) -> std::borrow::Cow<'_, str> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        id.into()
    } else {
        blake3::hash(id.as_bytes()).to_hex().to_string().into()
    }
}

pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn split_hash(hash: &str) -> (&str, &str) {
    if !hash.is_ascii() {
        return ("00", "00");
    }
    match hash.len() {
        0 => ("00", "00"),
        1 => (hash, "0"),
        2 | 3 => (&hash[..2], &hash[2..]),
        _ => (&hash[..2], &hash[2..4]),
    }
}

/// Keep extensions to a small, filesystem-safe alphabet.
pub fn sanitize_ext(ext: &str) -> String {
    let mut out = String::new();
    for ch in ext.trim_start_matches('.').chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
        if out.len() >= 8 {
            break;
        }
    }
    if out.is_empty() { "bin".into() } else { out }
}

#[cfg(test)]
mod hash_tests {
    use super::*;

    #[test]
    fn hash_sharding_preserves_existing_layout_and_handles_unicode() {
        assert_eq!(
            Paths::store_rel("abcdef0123", "JPG"),
            "store/ab/cd/abcdef0123.jpg"
        );
        assert_eq!(split_hash("a"), ("a", "0"));
        assert_eq!(split_hash("abc"), ("ab", "c"));
        for hash in ["ééé", "a💖", "🎉"] {
            assert_eq!(split_hash(hash), ("00", "00"));
        }
    }

    #[test]
    fn owned_paths_reject_escapes_and_generated_names_stay_safe() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(temp.path()).unwrap();
        for path in [
            "../victim",
            "store/../../victim",
            "library.db",
            "C:/victim",
            "store/file:stream",
            "store/file.",
        ] {
            assert!(paths.checked_owned_path(path).is_err(), "accepted {path}");
        }
        for id in ["../victim", "a/b", "C:/victim", "é💖"] {
            assert!(
                paths
                    .checked_owned_path(&Paths::store_rel(id, "jpg"))
                    .is_ok()
            );
            assert!(
                paths
                    .checked_owned_path(&Paths::preview_rel_paths(id)[0])
                    .is_ok()
            );
        }
    }
}
