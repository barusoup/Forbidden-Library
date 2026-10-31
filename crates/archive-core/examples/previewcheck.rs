use archive_core::{Library, TimelineFilter};
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).expect("library root");
    let library = Library::open(&root).expect("open library");
    for trashed in [false, true] {
        let items = library
            .items(&TimelineFilter {
                trashed,
                ..Default::default()
            })
            .expect("items");
        println!("{} items (trashed={trashed})", items.len());
        for item in items {
            let start = Instant::now();
            match library.load_preview_rgba(&item, 2048) {
                Ok((w, h, pixels)) => println!(
                    "ok   {} {}x{} {} bytes {:.0?} {}",
                    item.original_name,
                    w,
                    h,
                    pixels.len(),
                    start.elapsed(),
                    item.rel_path
                ),
                Err(error) => println!(
                    "FAIL {} {:.0?} {error} {}",
                    item.original_name,
                    start.elapsed(),
                    item.rel_path
                ),
            }
        }
    }
}
