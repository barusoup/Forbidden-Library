//! Storage benchmark for the parts of the engine a large library stresses.
//!
//! ```text
//! cargo run --release -p archive-core --example storebench
//! cargo run --release -p archive-core --example storebench -- --quick
//! ```
//!
//! The corpus is deterministic and split by size class, so runs on the same
//! machine can be compared directly. Imports use opaque files: this measures
//! the catalogue and the content-addressed store, not the codecs. Every
//! measured page is also checked against a single full read, and a duplicate
//! import must return the same id, so a faster number never comes from doing
//! less work.
//!
//! Results are written to `target/storebench/report.md` and `results.json`.

use archive_core::{ImportOptions, Library, TimelineFilter, human_bytes, import_paths};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIB: f64 = 1024.0 * 1024.0;

/// (label, bytes, count)
const CLASSES: [(&str, u64, usize); 3] = [
    ("64 KiB", 64 * 1024, 180),
    ("1 MiB + 13 B", 1024 * 1024 + 13, 48),
    ("8 MiB", 8 * 1024 * 1024, 12),
];
const ROWS: usize = 1000;
const PAGE: usize = 100;

struct Sample {
    workload: &'static str,
    op: String,
    unit: &'static str,
    values: Vec<f64>,
}

impl Sample {
    fn new(workload: &'static str, op: impl Into<String>, unit: &'static str) -> Self {
        Self {
            workload,
            op: op.into(),
            unit,
            values: Vec::new(),
        }
    }

    fn us(&mut self, elapsed: Duration) {
        self.values.push(elapsed.as_secs_f64() * 1e6);
    }

    fn value(&mut self, value: f64) {
        self.values.push(value);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|arg| arg == "--quick");
    let scale = if quick { 20 } else { 100 };

    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/storebench");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let started = Instant::now();
    let mut samples: Vec<Sample> = Vec::new();

    let corpus = build_corpus(&base.join("corpus"), scale);

    // --- unique import, one fresh library ---------------------------------
    let root = base.join("import");
    let library_root = root.join("library");
    std::fs::create_dir_all(&root).unwrap();
    let library = Library::open(&library_root).unwrap();
    let options = ImportOptions {
        include_other: true,
        cancel: None,
        optimize: None,
    };
    for (index, (label, bytes, _count)) in CLASSES.iter().enumerate() {
        let files = &corpus[index];
        // Batches share one worker pool, so the per-file figure is not
        // swamped by the cost of starting the pool for every file.
        let chunk = files.len().div_ceil(10).max(1);
        let mut sample = Sample::new("import unique", *label, "us/file");
        let mut total = Duration::ZERO;
        for group in files.chunks(chunk) {
            let started = Instant::now();
            let report = import_paths(&library, group, &options, &mut |_| {});
            let elapsed = started.elapsed();
            assert_eq!(report.imported, group.len(), "{report:?}");
            sample
                .values
                .push(elapsed.as_secs_f64() * 1e6 / group.len() as f64);
            total += elapsed;
        }
        let mibps = *bytes as f64 * files.len() as f64 / MIB / total.as_secs_f64();
        samples.push(sample);
        let mut rate = Sample::new("import unique", *label, "MiB/s");
        rate.value(mibps);
        samples.push(rate);
    }

    // --- duplicate import: hash only, no staging --------------------------
    let mut duplicate = Sample::new("import duplicate", "1 MiB, already stored", "us/file");
    for group in corpus[1].chunks(8) {
        let started = Instant::now();
        let report = import_paths(&library, group, &options, &mut |_| {});
        duplicate
            .values
            .push(started.elapsed().as_secs_f64() * 1e6 / group.len() as f64);
        assert_eq!(report.duplicates, group.len(), "{report:?}");
        assert_eq!(report.imported, 0, "{report:?}");
    }
    samples.push(duplicate);

    // --- catalogue: paging against a full read ----------------------------
    let rows = build_rows(&base.join("rows"), ROWS);
    for path in &rows {
        let report = import_paths(
            &library,
            std::slice::from_ref(path),
            &ImportOptions {
                include_other: true,
                cancel: None,
                optimize: None,
            },
            &mut |_| {},
        );
        assert_eq!(report.imported, 1);
    }
    let filter = TimelineFilter::default();
    let all = library.items(&filter).unwrap();
    let expected_rows = corpus.iter().map(Vec::len).sum::<usize>() + ROWS;
    assert_eq!(all.len(), expected_rows);

    let mut pages = Sample::new("catalogue", "keyset page (100 rows)", "us/page");
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let started = Instant::now();
        let page = library.item_page(&filter, cursor.as_ref(), PAGE).unwrap();
        pages.us(started.elapsed());
        assert!(page.total.is_none_or(|total| total == all.len()));
        if page.items.is_empty() {
            break;
        }
        seen.extend(page.items.iter().map(|item| item.id.clone()));
        cursor = page.cursor;
        if page.done {
            break;
        }
    }
    assert_eq!(
        seen,
        all.iter().map(|item| item.id.clone()).collect::<Vec<_>>()
    );
    samples.push(pages);

    let mut full = Sample::new("catalogue", "full read (all rows)", "us/read");
    for _ in 0..5 {
        let started = Instant::now();
        let rows = library.items(&filter).unwrap();
        full.us(started.elapsed());
        assert_eq!(rows.len(), all.len());
    }
    samples.push(full);

    // --- reopen ------------------------------------------------------------
    drop(library);
    let mut reopen = Sample::new("open", "open existing library", "ms/open");
    for _ in 0..5 {
        let started = Instant::now();
        let reopened = Library::open(&library_root).unwrap();
        reopen.values.push(started.elapsed().as_secs_f64() * 1e3);
        drop(reopened);
    }
    samples.push(reopen);
    let library = Library::open(&library_root).unwrap();

    // --- verify and backup ------------------------------------------------
    let verify_started = Instant::now();
    let report = library
        .verify(archive_core::VerifyOptions::default())
        .unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let mut verify = Sample::new("integrity", "verify full library", "ms");
    verify.value(verify_started.elapsed().as_secs_f64() * 1e3);
    samples.push(verify);

    let backup_started = Instant::now();
    let backup = library.backup(base.join("backup")).unwrap();
    let mut backup_sample = Sample::new("backup", "full library copy", "ms");
    backup_sample.value(backup_started.elapsed().as_secs_f64() * 1e3);
    samples.push(backup_sample);
    assert!(backup.items >= expected_rows);
    let restored = Library::open(&backup.path).unwrap();
    assert_eq!(
        restored.items(&filter).unwrap().len(),
        all.len(),
        "backup lost rows"
    );
    drop(restored);
    assert!(
        restored_verify(&backup.path),
        "backup did not verify as a library"
    );

    let report = build_report(&samples, started.elapsed(), &library_root, &backup.path);
    println!("{report}");
    std::fs::write(base.join("report.md"), &report).unwrap();
    std::fs::write(base.join("results.json"), samples_json(&samples)).unwrap();
    println!("saved: {}", base.join("report.md").display());
}

fn restored_verify(path: &str) -> bool {
    Library::open(path)
        .and_then(|library| library.verify(archive_core::VerifyOptions::default()))
        .map(|report| report.is_healthy())
        .unwrap_or(false)
}

/// Deterministic bytes per size class. Half the files are highly compressible
/// and half are not, so a corpus never accidentally favors one storage path.
fn build_corpus(dir: &Path, scale: usize) -> [Vec<PathBuf>; 3] {
    std::fs::create_dir_all(dir).unwrap();
    let mut classes: [Vec<PathBuf>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (index, (label, bytes, count)) in CLASSES.iter().enumerate() {
        let count = (count * scale / 100).max(2);
        for file in 0..count {
            let name = label.replace(' ', "-").replace('+', "plus");
            let path = dir.join(format!("{name}-{file:04}.bin"));
            let payload = payload(*bytes, index as u64 * 1000 + file as u64);
            std::fs::write(&path, payload).unwrap();
            classes[index].push(path);
        }
    }
    classes
}

fn payload(bytes: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes as usize);
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let compressible = seed.is_multiple_of(2);
    while (out.len() as u64) < bytes {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let byte = if compressible {
            ((state >> 56) as u8) & 0x0F
        } else {
            (state >> 56) as u8
        };
        out.push(byte);
    }
    out.truncate(bytes as usize);
    out
}

/// Small opaque rows for catalogue paging, long enough to be realistic and
/// unique enough not to deduplicate.
fn build_rows(dir: &Path, count: usize) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    (0..count)
        .map(|index| {
            let path = dir.join(format!("row-{index:04}.row"));
            let mut bytes = vec![0u8; 512];
            let text = format!("row {index}");
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            let seed = payload(64, index as u64 + 7);
            bytes[64..128].copy_from_slice(&seed);
            std::fs::write(&path, &bytes).unwrap();
            path
        })
        .collect()
}

fn stats(values: &[f64]) -> (f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite sample"));
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let p95 = sorted[(((n - 1) as f64) * 0.95).round() as usize];
    (median, p95)
}

fn fmt_num(value: f64) -> String {
    if value >= 1000.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn build_report(samples: &[Sample], elapsed: Duration, library: &Path, backup: &str) -> String {
    let mut out = String::new();
    out.push_str("# storebench results\n\n");
    out.push_str(&format!(
        "total wall time: {:.1}s; each op reports median / p95 over its samples.\n\n",
        elapsed.as_secs_f64()
    ));
    out.push_str("| workload | op | unit | median | p95 | n |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for sample in samples {
        let (median, p95) = stats(&sample.values);
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            sample.workload,
            sample.op,
            sample.unit,
            fmt_num(median),
            fmt_num(p95),
            sample.values.len()
        ));
    }
    out.push_str("\nLibrary size: ");
    out.push_str(&human_bytes(dir_bytes(library)));
    out.push_str(&format!("\nBackup: {backup}\n"));
    out
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(metadata) = entry.metadata() {
                total += metadata.len();
            }
        }
    }
    total
}

fn samples_json(samples: &[Sample]) -> String {
    let mut out = String::from("[\n");
    for (index, sample) in samples.iter().enumerate() {
        let (median, p95) = stats(&sample.values);
        out.push_str(&format!(
            "  {{\"workload\":\"{}\",\"op\":\"{}\",\"unit\":\"{}\",\
             \"n\":{},\"median\":{median},\"p95\":{p95}}}",
            sample.workload,
            sample.op.replace('"', "'"),
            sample.unit,
            sample.values.len()
        ));
        if index + 1 != samples.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}
