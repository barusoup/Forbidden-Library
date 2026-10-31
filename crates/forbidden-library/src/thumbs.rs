use dream::gfx::vulkan::GpuContext;
use dream::texture::{GpuTexture, TextureOptions};
use std::collections::HashMap;
use std::sync::Arc;

struct Entry<T> {
    texture: Arc<T>,
    used: u64,
}

/// A small LRU of uploaded previews. A thousand visible tiles would cost
/// hundreds of megabytes of VRAM, so only the working set stays resident;
/// evicted textures wait three frames before dropping so a frame still
/// referencing them is never freed under the GPU.
///
/// The value type is generic only so the eviction rules can be tested without
/// a GPU; the app always uses `GpuTexture`.
pub struct ThumbCache<T = GpuTexture> {
    entries: HashMap<String, Entry<T>>,
    retired: Vec<(Arc<T>, u8)>,
    max: usize,
    tick: u64,
}

impl<T> ThumbCache<T> {
    pub fn new(max: usize) -> Self {
        Self {
            entries: HashMap::new(),
            retired: Vec::new(),
            max: max.max(1),
            tick: 0,
        }
    }

    /// Make room for at least `max` entries. The cache only ever grows, so a
    /// session that has shown many tiles keeps them rather than thrashing.
    pub fn ensure_capacity(&mut self, max: usize) {
        self.max = self.max.max(max);
    }

    pub fn get(&mut self, key: &str) -> Option<Arc<T>> {
        self.tick += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.tick;
        Some(Arc::clone(&entry.texture))
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn insert(&mut self, key: String, texture: T) {
        self.tick += 1;
        if let Some(old) = self.entries.remove(&key) {
            self.retired.push((old.texture, 3));
        }
        if self.entries.len() >= self.max
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            && let Some(entry) = self.entries.remove(&oldest)
        {
            self.retired.push((entry.texture, 3));
        }
        self.entries.insert(
            key,
            Entry {
                texture: Arc::new(texture),
                used: self.tick,
            },
        );
    }

    /// Once per frame, before rendering.
    pub fn retire_frame(&mut self) {
        for (_, frames) in self.retired.iter_mut() {
            *frames = frames.saturating_sub(1);
        }
        self.retired.retain(|(_, frames)| *frames > 0);
    }

    pub fn clear(&mut self) {
        for (_, entry) in self.entries.drain() {
            self.retired.push((entry.texture, 3));
        }
    }
}

/// `mip_levels` of `None` builds the full chain, which a large image needs
/// when it is drawn well below its decoded size.
pub fn upload(
    context: &Arc<GpuContext>,
    label: &str,
    width: u32,
    height: u32,
    pixels: &[u8],
    mip_levels: Option<u32>,
) -> Option<GpuTexture> {
    if width == 0 || height == 0 || pixels.len() != width as usize * height as usize * 4 {
        return None;
    }
    Some(GpuTexture::from_rgba8(
        context,
        label,
        pixels,
        width,
        height,
        TextureOptions {
            mip_levels,
            ..TextureOptions::default()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_within_capacity_are_never_evicted() {
        let mut cache: ThumbCache<u32> = ThumbCache::new(2);
        cache.insert("a".into(), 1);
        cache.insert("b".into(), 2);
        cache.ensure_capacity(4);
        cache.insert("c".into(), 3);
        cache.insert("d".into(), 4);
        for key in ["a", "b", "c", "d"] {
            assert!(cache.contains(key), "{key} was evicted within capacity");
        }
    }

    #[test]
    fn capacity_never_shrinks() {
        let mut cache: ThumbCache<u32> = ThumbCache::new(8);
        cache.ensure_capacity(3);
        for index in 0..8 {
            cache.insert(index.to_string(), index);
        }
        assert_eq!(cache.entries.len(), 8);
    }
}
