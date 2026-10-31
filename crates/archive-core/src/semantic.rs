//! Semantic search over the catalogue: unit-length image vectors and exact
//! cosine ranking.
//!
//! Vectors are stored once per item, truncated to [`EMBED_DIMS`] dimensions
//! and L2-normalized before they reach this module, so a dot product is the
//! cosine similarity. Scanning every vector is deliberate: a few hundred
//! thousand items is a handful of milliseconds, while an approximate index
//! would need tuning, rebuilds and a second source of truth.
//!
//! Storage is one contiguous buffer rather than a `Vec` per item. That is
//! what makes the scan cheap: a query walks memory in order instead of
//! chasing a pointer per photo, and a large library can be split across
//! threads with nothing more than a slice.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

/// Identifier of the bundled model and prompt revision. Changing either
/// invalidates stored vectors and re-indexes the archive.
pub const EMBED_MODEL: &str = "embeddinggemma-2-ud-q4-256";

/// Vector width. The model is trained for Matryoshka truncation; 256 keeps
/// retrieval quality close to the full 768 while cutting storage and scan
/// cost by three.
pub const EMBED_DIMS: usize = 256;

/// Below this many vectors, starting threads costs more than the scan.
const PARALLEL_MIN_ITEMS: usize = 8_192;
/// Upper bound on scan threads: the work is memory-bandwidth bound well
/// before it is core bound.
const MAX_SCAN_THREADS: usize = 8;

/// Scale a raw model vector into the stored space: keep the leading
/// dimensions, reject non-finite values, and renormalize (truncating a unit
/// vector does not preserve its length).
pub fn prepare(vector: &[f32], dims: usize) -> Vec<f32> {
    if dims == 0 || vector.len() < dims {
        return Vec::new();
    }
    let mut out = vector[..dims].to_vec();
    if !out.iter().all(|value| value.is_finite()) {
        return Vec::new();
    }
    normalize(&mut out);
    out
}

pub fn normalize(vector: &mut [f32]) {
    let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if length.is_finite() && length > 1e-6 {
        for value in vector {
            *value /= length;
        }
    }
}

pub struct VectorIndex {
    dims: usize,
    ids: Vec<String>,
    /// `ids.len() * dims` values: item `n` occupies `n*dims..(n+1)*dims`.
    vectors: Vec<f32>,
    // Position of each id, so indexing an archive is linear rather than
    // quadratic in the number of items.
    positions: HashMap<String, usize>,
}

impl VectorIndex {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            ids: Vec::new(),
            vectors: Vec::new(),
            positions: HashMap::new(),
        }
    }

    pub fn from_rows(dims: usize, rows: Vec<(String, Vec<f32>)>) -> Self {
        let mut index = Self::new(dims);
        index.ids.reserve(rows.len());
        index.vectors.reserve(rows.len().saturating_mul(dims));
        for (id, vector) in rows {
            index.insert(id, vector);
        }
        index
    }

    /// Build straight from the layout the catalogue stores: a list of ids and
    /// one contiguous buffer of vectors.
    pub fn from_flat(dims: usize, ids: Vec<String>, vectors: Vec<f32>) -> Self {
        let usable = vectors
            .len()
            .checked_div(dims)
            .map(|complete| ids.len().min(complete))
            .unwrap_or(0);
        let mut ids = ids;
        let mut vectors = vectors;
        ids.truncate(usable);
        vectors.truncate(usable * dims);
        let mut positions = HashMap::with_capacity(usable);
        for (position, id) in ids.iter().enumerate() {
            positions.insert(id.clone(), position);
        }
        Self {
            dims,
            ids,
            vectors,
            positions,
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn insert(&mut self, id: String, vector: Vec<f32>) {
        if self.dims == 0 || vector.len() != self.dims || vector.is_empty() {
            return;
        }
        match self.positions.get(&id) {
            Some(position) => {
                let start = *position * self.dims;
                self.vectors[start..start + self.dims].copy_from_slice(&vector);
            }
            None => {
                self.positions.insert(id.clone(), self.ids.len());
                self.ids.push(id);
                self.vectors.extend_from_slice(&vector);
            }
        }
    }

    /// The `limit` closest items at or above `min_score`, best first.
    pub fn search(&self, query: &[f32], limit: usize, min_score: f32) -> Vec<(String, f32)> {
        if query.len() != self.dims {
            return Vec::new();
        }
        self.ranked(query, limit, min_score)
    }

    /// Rank by how much closer the query is than a neutral baseline for the
    /// same media. This model gives every image a high, shared similarity to
    /// any text, so the raw cosine barely separates "a black cat" from
    /// "asdfgh". Subtracting the baseline similarity removes that shared
    /// component and leaves the part specific to the query.
    ///
    /// `dot(query, v) - dot(baseline, v)` is `dot(query - baseline, v)`, so
    /// the difference is folded once and every item costs one dot product
    /// instead of two.
    pub fn search_relative(
        &self,
        query: &[f32],
        baseline: &[f32],
        limit: usize,
        min_delta: f32,
    ) -> Vec<(String, f32)> {
        if query.len() != self.dims || baseline.len() != self.dims {
            return Vec::new();
        }
        let needle: Vec<f32> = query
            .iter()
            .zip(baseline)
            .map(|(query, baseline)| query - baseline)
            .collect();
        self.ranked(&needle, limit, min_delta)
    }

    fn ranked(&self, needle: &[f32], limit: usize, min_score: f32) -> Vec<(String, f32)> {
        if limit == 0 || self.ids.is_empty() || needle.len() != self.dims {
            return Vec::new();
        }
        let hits = if self.ids.len() >= PARALLEL_MIN_ITEMS {
            self.ranked_parallel(needle, limit, min_score)
        } else {
            self.ranked_serial(needle, limit, min_score)
        };
        hits.into_iter()
            .map(|hit| (self.ids[hit.index].clone(), hit.score))
            .collect()
    }

    fn ranked_serial(&self, needle: &[f32], limit: usize, min_score: f32) -> Vec<Hit> {
        let mut best = BinaryHeap::with_capacity(limit + 1);
        for (index, vector) in self.vectors.chunks_exact(self.dims).enumerate() {
            let score = dot(needle, vector);
            if !score.is_finite() || score < min_score {
                continue;
            }
            push_bounded(&mut best, limit, Hit { score, index });
        }
        sorted(best)
    }

    fn ranked_parallel(&self, needle: &[f32], limit: usize, min_score: f32) -> Vec<Hit> {
        let cores = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(4);
        let threads = cores.min(MAX_SCAN_THREADS).min(self.ids.len());
        if threads <= 1 {
            return self.ranked_serial(needle, limit, min_score);
        }
        let chunk = self.ids.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .vectors
                .chunks(chunk * self.dims)
                .enumerate()
                .map(|(chunk_index, slice)| {
                    let offset = chunk_index * chunk;
                    scope.spawn(move || {
                        let mut best = BinaryHeap::with_capacity(limit + 1);
                        for (local, vector) in slice.chunks_exact(self.dims).enumerate() {
                            let score = dot(needle, vector);
                            if !score.is_finite() || score < min_score {
                                continue;
                            }
                            push_bounded(
                                &mut best,
                                limit,
                                Hit {
                                    score,
                                    index: offset + local,
                                },
                            );
                        }
                        best
                    })
                })
                .collect();
            let mut best = BinaryHeap::with_capacity(limit + 1);
            for handle in handles {
                let partial = handle.join().expect("vector scan worker panicked");
                for hit in partial {
                    push_bounded(&mut best, limit, hit);
                }
            }
            sorted(best)
        })
    }
}

/// Keep only the `limit` best hits. The heap's top is the worst kept hit.
fn push_bounded(best: &mut BinaryHeap<Hit>, limit: usize, hit: Hit) {
    if best.len() < limit {
        best.push(hit);
    } else if let Some(worst) = best.peek()
        && hit > *worst
    {
        best.pop();
        best.push(hit);
    }
}

fn sorted(heap: BinaryHeap<Hit>) -> Vec<Hit> {
    let mut hits = heap.into_vec();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.index.cmp(&right.index))
    });
    hits
}

/// Dot product with four independent accumulators. Each one is a separate
/// dependency chain, so the compiler can keep the pipeline full and vectorize
/// the loop instead of waiting on a single serial sum.
fn dot(left: &[f32], right: &[f32]) -> f32 {
    let length = left.len().min(right.len());
    let (left, right) = (&left[..length], &right[..length]);
    let chunks = length / 4;
    let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for index in 0..chunks {
        let at = index * 4;
        s0 += left[at] * right[at];
        s1 += left[at + 1] * right[at + 1];
        s2 += left[at + 2] * right[at + 2];
        s3 += left[at + 3] * right[at + 3];
    }
    let mut sum = (s0 + s1) + (s2 + s3);
    for index in chunks * 4..length {
        sum += left[index] * right[index];
    }
    sum
}

struct Hit {
    score: f32,
    index: usize,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.index == other.index
    }
}

impl Eq for Hit {}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher score wins; on a tie the earlier item is preferred, so the
        // later one is the one dropped from the heap.
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.index.cmp(&other.index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(values: &[f32]) -> Vec<f32> {
        let mut vector = values.to_vec();
        normalize(&mut vector);
        vector
    }

    #[test]
    fn truncation_keeps_a_unit_vector() {
        let raw = vec![0.6, 0.8, 0.3, 0.1];
        let prepared = prepare(&raw, 2);
        assert_eq!(prepared.len(), 2);
        let length = (prepared[0] * prepared[0] + prepared[1] * prepared[1]).sqrt();
        assert!((length - 1.0).abs() < 1e-6);
    }

    #[test]
    fn prepare_rejects_broken_vectors() {
        assert!(prepare(&[f32::NAN, 1.0], 2).is_empty());
        assert!(prepare(&[1.0], 2).is_empty());
        assert!(prepare(&[1.0, 0.0], 0).is_empty());
    }

    #[test]
    fn search_orders_by_cosine_and_honours_the_cutoff() {
        let mut index = VectorIndex::new(3);
        index.insert("cat".into(), vector(&[1.0, 0.0, 0.0]));
        index.insert("dog".into(), vector(&[0.9, 0.1, 0.0]));
        index.insert("boat".into(), vector(&[0.0, 1.0, 0.0]));
        let hits = index.search(&vector(&[1.0, 0.0, 0.0]), 2, 0.5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "cat");
        assert_eq!(hits[1].0, "dog");
        assert!(hits[0].1 > hits[1].1);
        assert!(index.search(&vector(&[0.0, 0.0, 1.0]), 5, 0.5).is_empty());
    }

    #[test]
    fn relative_search_removes_the_shared_similarity() {
        // Every image has a strong generic similarity to the query; only
        // "cat" clears the baseline by a meaningful amount.
        let mut index = VectorIndex::new(3);
        index.insert("cat".into(), vector(&[1.0, 0.0, 0.0]));
        index.insert("boat".into(), vector(&[0.6, 0.8, 0.0]));
        let query = vector(&[1.0, 0.0, 0.0]);
        let baseline = vector(&[0.8, 0.2, 0.0]);
        let raw = index.search(&query, 5, 0.0);
        assert_eq!(raw.len(), 2, "both look similar without the baseline");
        let hits = index.search_relative(&query, &baseline, 5, 0.02);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "cat");
        assert!(index.search_relative(&query, &baseline, 5, 0.9).is_empty());
    }

    #[test]
    fn relative_search_scores_match_dot_differences() {
        // The folded needle has to rank exactly like the definition it
        // replaces: dot(query, v) - dot(baseline, v).
        let mut index = VectorIndex::new(8);
        let mut seed = 0x2545_f491u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16_777_216.0 - 0.5
        };
        for item in 0..200 {
            let raw: Vec<f32> = (0..8).map(|_| next()).collect();
            index.insert(format!("item-{item}"), vector(&raw));
        }
        let query = vector(&(0..8).map(|_| next()).collect::<Vec<_>>());
        let baseline = vector(&(0..8).map(|_| next()).collect::<Vec<_>>());
        let definition = |item: usize| -> f64 {
            let at = item * 8;
            let stored = &index.vectors[at..at + 8];
            let dot64 = |left: &[f32], right: &[f32]| -> f64 {
                left.iter()
                    .zip(right)
                    .map(|(a, b)| *a as f64 * *b as f64)
                    .sum()
            };
            dot64(&query, stored) - dot64(&baseline, stored)
        };
        let hits = index.search_relative(&query, &baseline, 5, -1.0);
        assert_eq!(hits.len(), 5);
        // Every reported score is the definitional one, within float noise.
        for (id, score) in &hits {
            let item: usize = id.strip_prefix("item-").unwrap().parse().unwrap();
            let expected = definition(item);
            assert!(
                (*score as f64 - expected).abs() < 5e-4,
                "{id}: {score} vs {expected}"
            );
        }
        // The best hit really is the best item, and the list never rises.
        let best = (0..200).map(definition).fold(f64::NEG_INFINITY, f64::max);
        assert!((hits[0].1 as f64 - best).abs() < 5e-4);
        for pair in hits.windows(2) {
            assert!(pair[0].1 >= pair[1].1);
        }
    }

    #[test]
    fn the_parallel_scan_agrees_with_the_serial_one() {
        let dims = 16;
        let mut index = VectorIndex::new(dims);
        let mut seed = 0x9e37_79b9u32;
        for item in 0..(PARALLEL_MIN_ITEMS + 128) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let raw: Vec<f32> = (0..dims)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 8) as f32 / 16_777_216.0 - 0.5
                })
                .collect();
            index.insert(format!("{item}"), vector(&raw));
        }
        let needle = vector(&(0..dims).map(|_| 0.25).collect::<Vec<_>>());
        let serial = index.ranked_serial(&needle, 25, -1.0);
        let parallel = index.ranked_parallel(&needle, 25, -1.0);
        assert_eq!(serial.len(), parallel.len());
        for (left, right) in serial.iter().zip(parallel.iter()) {
            assert_eq!(left.index, right.index);
            assert_eq!(left.score, right.score);
        }
    }

    #[test]
    fn inserting_an_item_twice_replaces_it() {
        let mut index = VectorIndex::new(2);
        index.insert("a".into(), vector(&[1.0, 0.0]));
        index.insert("a".into(), vector(&[0.0, 1.0]));
        assert_eq!(index.len(), 1);
        let hits = index.search(&vector(&[0.0, 1.0]), 5, 0.0);
        assert_eq!(hits[0].0, "a");
        assert!((hits[0].1 - 1.0).abs() < 1e-6);
    }
}
