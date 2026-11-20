//! Start-up self-update.
//!
//! Before any window exists, an installed copy asks GitHub whether a newer
//! release is published. If one is, its update archive is downloaded,
//! checked against the published SHA-256, swapped into the install folder and
//! the new executable is started in place of this one. When the machine is
//! offline, or anything at all goes wrong, the current version simply starts.
//!
//! Each platform has its own manifest next to the release assets:
//!
//! ```json
//! { "version": "0.1.1", "url": "https://github.com/.../update.zip",
//!   "sha256": "<hex>", "size": 12345678 }
//! ```
//!
//! Windows publishes `update.json` and `update.zip`; the Inno Setup installer
//! leaves `unins000.exe` beside the executable, and only a copy with that
//! file updates itself. Linux publishes `update-linux.json` and
//! `update-linux.zip`; `installer/install.sh` writes a `.installed` marker in
//! the install directory instead. The marker matters there because a Linux
//! build can be unpacked and run from anywhere: without it, a loose copy next
//! to a `library/` folder would rewrite itself and its neighbours.
//!
//! The archive holds the files that change between releases. The large,
//! rarely changing parts (FFmpeg, the search model) arrive with the installer.
//! Files in use cannot be deleted on Windows but can be renamed, so replaced
//! files are moved into `.update-old` and removed on a later launch. Linux can
//! replace a running executable outright, but the same staged swap is used for
//! both platforms so there is one code path and one rollback story.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
const PLATFORM: &str = "Windows";
#[cfg(not(windows))]
const PLATFORM: &str = "Linux";

/// Manifest and archive a release publishes for this platform.
#[cfg(windows)]
const MANIFEST_URL: &str =
    "https://github.com/barusoup/Forbidden-Library/releases/latest/download/update.json";
#[cfg(not(windows))]
const MANIFEST_URL: &str =
    "https://github.com/barusoup/Forbidden-Library/releases/latest/download/update-linux.json";
#[cfg(windows)]
const ARCHIVE_FILE: &str = "update.zip";
#[cfg(not(windows))]
const ARCHIVE_FILE: &str = "update-linux.zip";
/// File the installer leaves in the install directory; its presence is what
/// makes a copy of the app eligible to update itself.
#[cfg(windows)]
const INSTALL_MARKER: &str = "unins000.exe";
#[cfg(not(windows))]
const INSTALL_MARKER: &str = ".installed";

const DOWNLOAD_PREFIX: &str = "https://github.com/barusoup/Forbidden-Library/releases/download/";
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
const OLD_DIR: &str = ".update-old";
const STAGING_DIR: &str = ".update-staging";

#[derive(Deserialize)]
struct Manifest {
    version: String,
    url: String,
    sha256: String,
    size: u64,
}

/// Runs the update check. Returns `true` when a newer copy was installed and
/// started, in which case the caller must exit without opening a window.
pub fn run_before_launch(args: &[String]) -> bool {
    if cfg!(debug_assertions)
        || std::env::var_os("FORBIDDEN_LIBRARY_NO_UPDATE").is_some()
        || args.iter().any(|arg| arg.starts_with("--"))
    {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(install) = exe.parent().map(Path::to_path_buf) else {
        return false;
    };
    // Only a copy placed by the platform installer updates itself: the
    // Windows installer leaves `unins000.exe`, and install.sh leaves
    // `.installed` in the install directory.
    if !installed(&install) {
        return false;
    }
    let _ = fs::remove_dir_all(install.join(OLD_DIR));
    match update(&exe, &install, args) {
        Ok(started) => started,
        Err(error) => {
            log(&format!("{PLATFORM} update skipped: {error}"));
            false
        }
    }
}

/// Whether this copy was put in place by the installer. A loose unpacked
/// build must never rewrite itself.
fn installed(install: &Path) -> bool {
    install.join(INSTALL_MARKER).is_file()
}

fn update(exe: &Path, install: &Path, args: &[String]) -> Result<bool, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(1500))
        .timeout_read(Duration::from_secs(15))
        .build();
    let manifest: Manifest = agent
        .get(MANIFEST_URL)
        .timeout(Duration::from_secs(4))
        .call()
        .map_err(|e| format!("manifest: {e}"))?
        .into_json()
        .map_err(|e| format!("manifest: {e}"))?;

    if !is_newer(&manifest.version, env!("CARGO_PKG_VERSION")) {
        return Ok(false);
    }
    if !archive_url_is_trusted(&manifest.url) || manifest.size == 0 || manifest.size > MAX_ARCHIVE {
        return Err("manifest points somewhere unexpected".into());
    }
    if recently_tried(&manifest.version) {
        return Ok(false);
    }
    note_attempt(&manifest.version);

    let work = crate::settings::Settings::data_dir().join("updates");
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let archive = work.join(format!("{}.zip", manifest.version));
    let result = download(&agent, &manifest, &archive).and_then(|()| {
        let staging = install.join(STAGING_DIR);
        let _ = fs::remove_dir_all(&staging);
        let outcome = extract(&archive, &staging)
            .and_then(|()| swap_in(&staging, install, &install.join(OLD_DIR)));
        let _ = fs::remove_dir_all(&staging);
        outcome
    });
    let _ = fs::remove_file(&archive);
    result?;

    // Zip entries do not always carry the executable bit. Make sure the
    // replacement can actually be started before handing over to it.
    ensure_executable(exe);
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .map_err(|e| format!("restart: {e}"))?;
    Ok(true)
}

/// A release archive must come from this project's downloads and carry this
/// platform's archive name; anything else is a redirect or a bad manifest.
fn archive_url_is_trusted(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.starts_with(DOWNLOAD_PREFIX) && path.rsplit('/').next() == Some(ARCHIVE_FILE)
}

#[cfg(unix)]
fn ensure_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o111 == 0 {
            permissions.set_mode(0o755);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) {}

fn download(agent: &ureq::Agent, manifest: &Manifest, dest: &Path) -> Result<(), String> {
    let response = agent
        .get(&manifest.url)
        .call()
        .map_err(|e| format!("download: {e}"))?;
    let mut reader = response.into_reader().take(manifest.size + 1);
    let mut file = fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|e| format!("download: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buffer[..n]);
        file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())?;
    if total != manifest.size {
        return Err("download has the wrong size".into());
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !digest.eq_ignore_ascii_case(manifest.sha256.trim()) {
        return Err("download does not match its checksum".into());
    }
    Ok(())
}

fn extract(archive: &Path, staging: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| e.to_string())?;
        let Some(relative) = entry.enclosed_name() else {
            return Err("archive holds an unsafe path".into());
        };
        let target = staging.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&target).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        // Unix archives record permissions; keep helper binaries executable.
        // Files without a recorded mode keep the umask default.
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777));
        }
    }
    Ok(())
}

/// Moves every staged file into the install folder, parking whatever it
/// replaces in `old`. If any step fails, everything already moved is undone.
fn swap_in(staging: &Path, install: &Path, old: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect(staging, Path::new(""), &mut files).map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err("archive is empty".into());
    }
    fs::create_dir_all(old).map_err(|e| e.to_string())?;
    // (installed path, parked copy of what was there before, if anything)
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut apply = || -> std::io::Result<()> {
        for (number, relative) in files.iter().enumerate() {
            let target = install.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let parked = if target.exists() {
                let name = target.file_name().unwrap_or_default().to_string_lossy();
                let parked = old.join(format!("{number}-{name}"));
                fs::rename(&target, &parked)?;
                Some(parked)
            } else {
                None
            };
            done.push((target.clone(), parked));
            fs::rename(staging.join(relative), &target)?;
        }
        Ok(())
    };
    if let Err(error) = apply() {
        for (target, parked) in done.into_iter().rev() {
            let _ = fs::remove_file(&target);
            if let Some(parked) = parked {
                let _ = fs::rename(parked, target);
            }
        }
        return Err(format!("could not replace files: {error}"));
    }
    Ok(())
}

fn collect(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            collect(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_version(text: &str) -> Option<[u64; 3]> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn attempt_file() -> PathBuf {
    crate::settings::Settings::data_dir().join("update-attempt")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A failed install must not be retried on every launch.
fn recently_tried(version: &str) -> bool {
    let Ok(text) = fs::read_to_string(attempt_file()) else {
        return false;
    };
    let Some((tried, at)) = text.trim().split_once(' ') else {
        return false;
    };
    tried == version
        && at
            .parse::<u64>()
            .is_ok_and(|at| now().saturating_sub(at) < RETRY_AFTER.as_secs())
}

fn note_attempt(version: &str) {
    let path = attempt_file();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format!("{version} {}", now()));
}

fn log(line: &str) {
    let dir = crate::settings::Settings::data_dir().join("logs");
    if fs::create_dir_all(&dir).is_ok() {
        let _ = fs::write(dir.join("update.log"), format!("{line}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("junk", "0.1.0"));
        assert!(!is_newer("1.0", "0.1.0"));
    }

    #[test]
    fn only_installer_placed_copies_update_themselves() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!installed(dir.path()));
        fs::write(dir.path().join(INSTALL_MARKER), "").unwrap();
        assert!(installed(dir.path()));
    }

    #[test]
    fn selection_uses_this_platforms_manifest_and_archive() {
        #[cfg(not(windows))]
        {
            assert!(MANIFEST_URL.ends_with("/update-linux.json"));
            assert_eq!(ARCHIVE_FILE, "update-linux.zip");
            assert_eq!(INSTALL_MARKER, ".installed");
        }
        #[cfg(windows)]
        {
            assert!(MANIFEST_URL.ends_with("/update.json"));
            assert_eq!(ARCHIVE_FILE, "update.zip");
            assert_eq!(INSTALL_MARKER, "unins000.exe");
        }
        assert!(archive_url_is_trusted(&format!(
            "{DOWNLOAD_PREFIX}v1.2.3/{ARCHIVE_FILE}"
        )));
        let other = if cfg!(windows) {
            "update-linux.zip"
        } else {
            "update.zip"
        };
        assert!(!archive_url_is_trusted(&format!(
            "{DOWNLOAD_PREFIX}v1.2.3/{other}"
        )));
        assert!(!archive_url_is_trusted("https://example.com/update.zip"));
        assert!(!archive_url_is_trusted(""));
    }

    #[test]
    fn swap_replaces_files_and_parks_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (install, staging, old) = (
            dir.path().join("app"),
            dir.path().join("stage"),
            dir.path().join("old"),
        );
        fs::create_dir_all(install.join("jxl")).unwrap();
        fs::create_dir_all(staging.join("jxl")).unwrap();
        fs::write(install.join("app.exe"), "old").unwrap();
        fs::write(install.join("keep.bin"), "keep").unwrap();
        fs::write(staging.join("app.exe"), "new").unwrap();
        fs::write(staging.join("jxl").join("tool.exe"), "tool").unwrap();

        swap_in(&staging, &install, &old).unwrap();

        assert_eq!(fs::read_to_string(install.join("app.exe")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(install.join("keep.bin")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_to_string(install.join("jxl").join("tool.exe")).unwrap(),
            "tool"
        );
        assert_eq!(fs::read_dir(&old).unwrap().count(), 1);
    }
}
