use crate::Library;
use crate::db;
use crate::error::Result;
use crate::model::VerifyReport;
use crate::store;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug)]
pub struct VerifyOptions {
    /// Re-hash every stored file (slow, definitive).
    pub hashes: bool,
    /// Confirm previews exist.
    pub thumbs: bool,
    /// Report store files the catalogue does not know about.
    pub orphans: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            hashes: true,
            thumbs: true,
            orphans: true,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct RowCheck {
    missing: bool,
    corrupt: bool,
    missing_thumb: bool,
}

impl RowCheck {
    fn healthy(self) -> bool {
        !self.missing && !self.corrupt && !self.missing_thumb
    }
}

pub fn run(library: &Library, options: VerifyOptions) -> Result<VerifyReport> {
    // Verify a stable catalogue/store pair, rather than reporting files
    // replaced by a concurrent optimization as missing or corrupt.
    let _files = library.files.write().unwrap_or_else(|e| e.into_inner());
    let started = std::time::Instant::now();
    let mut report = VerifyReport::default();
    let rows = {
        let conn = library.conn();
        report.database_ok = db::database_check(&conn);
        db::verify_rows(&conn)?
    };
    report.checked = rows.len();

    // Hashing a big library is entirely CPU and disk throughput, and every
    // file is independent, so the walk runs on a bounded pool instead of one
    // file after another.
    let checks = check_all(library, &rows, options);
    let mut known: HashSet<&str> = HashSet::with_capacity(rows.len());
    for (row, check) in rows.iter().zip(&checks) {
        known.insert(row.rel_path.as_str());
        if check.missing {
            report.missing.push(row.rel_path.clone());
            continue;
        }
        if check.corrupt {
            report.corrupt.push(row.rel_path.clone());
        }
        if check.missing_thumb {
            report.missing_thumbs.push(row.id.clone());
        }
        if check.healthy() {
            report.ok += 1;
        }
    }

    if options.orphans {
        let mut stack = vec![library.paths().store().to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if std::fs::symlink_metadata(&path)
                    .map(|meta| crate::paths::is_link(&meta))
                    .unwrap_or(true)
                {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(library.root()) else {
                    continue;
                };
                let rel = relative.to_string_lossy().replace('\\', "/");
                if !known.contains(rel.as_str()) {
                    report.orphan_files += 1;
                    report.orphan_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

fn check_all(library: &Library, rows: &[db::VerifyRow], options: VerifyOptions) -> Vec<RowCheck> {
    let mut checks = vec![RowCheck::default(); rows.len()];
    if rows.is_empty() {
        return checks;
    }
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4);
    let threads = cores.min(8).min(rows.len());
    if threads <= 1 {
        for (check, row) in checks.iter_mut().zip(rows) {
            *check = check_row(library, row, options);
        }
        return checks;
    }
    let chunk = rows.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = rows
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    let mut local = Vec::with_capacity(slice.len());
                    for row in slice {
                        local.push(check_row(library, row, options));
                    }
                    local
                })
            })
            .collect();
        for (index, handle) in handles.into_iter().enumerate() {
            let start = index * chunk;
            let end = (start + chunk).min(rows.len());
            match handle.join() {
                Ok(local) => checks[start..end].copy_from_slice(&local),
                // A worker that panicked verified nothing. Its rows are reported
                // as corrupt, so the report says so rather than passing them or
                // taking the whole check down.
                Err(_) => mark_unverified(&mut checks[start..end]),
            }
        }
    });
    checks
}

/// Rows that no worker checked are reported as corrupt. Passing them would
/// claim a verification that never happened.
fn mark_unverified(checks: &mut [RowCheck]) {
    for check in checks {
        check.corrupt = true;
    }
}

fn check_row(library: &Library, row: &db::VerifyRow, options: VerifyOptions) -> RowCheck {
    let mut check = RowCheck::default();
    let path = library.paths().abs(&row.rel_path);
    if !path.is_file() {
        check.missing = true;
        return check;
    }
    if options.hashes {
        match store::hash_file(&path) {
            Ok(hash) if hash == row.checksum => {}
            _ => check.corrupt = true,
        }
    }
    if options.thumbs
        && row.thumb_state == "ready"
        && !library.paths().thumb_path(&row.id).is_file()
    {
        check.missing_thumb = true;
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_of_a_panicked_worker_are_reported_as_corrupt() {
        let mut checks = [RowCheck::default(); 3];
        mark_unverified(&mut checks[1..]);
        assert!(checks[0].healthy());
        assert!(!checks[1].healthy());
        assert!(!checks[2].healthy());
    }
}
