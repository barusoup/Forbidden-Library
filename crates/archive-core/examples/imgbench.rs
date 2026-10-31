use image::imageops::FilterType;
use std::time::Instant;

fn main() {
    for path in std::env::args().skip(1) {
        let start = Instant::now();
        let image = image::open(&path).expect("decode");
        let decoded = start.elapsed();
        let (w, h) = (image.width(), image.height());
        let start = Instant::now();
        let resized = image.resize(2048, 2048, FilterType::Lanczos3);
        println!(
            "{path}: {w}x{h} decode {decoded:.0?} lanczos3 {:.0?} -> {}x{}",
            start.elapsed(),
            resized.width(),
            resized.height()
        );
    }
}
