//! The bounded texture cache.
//!
//! A five-year library has hundreds of thousands of thumbnails. Upstream sidesteps the problem by
//! letting the browser cache them; a native process has no such out, and 200 KB of RGBA per card
//! times every page ever looked at is an afternoon of scrolling away. So the cache is capped, and
//! eviction is explicit.
//!
//! Dropping a `TextureHandle` is what frees the GPU texture (`epaint`'s `Drop` sends a free op to
//! the texture manager), so eviction is just removing the entry — no second bookkeeping.

use std::collections::{HashMap, VecDeque};

use crate::model::{DecodedImage, RowKey};

/// How many thumbnails may sit resident. 2000 × ~11 KB of RGBA is ~22 MB, which is a rounding error
/// next to a renderer and a real cap on growth.
pub const CAP: usize = 2000;

/// How many *full* frames may sit resident, for the overlay a card click opens.
///
/// The number is arithmetic rather than taste: a 1080p RGBA frame is ~8 MB, so the 2000-entry thumbnail
/// cap would ask for 16 GB. Three covers the picture being looked at plus the one being decoded — as far
/// ahead as a click can get — plus the moving picture the player is drawing, which has to be a third
/// entry and not a replacement for either of the first two: the still of a row and the video of that same
/// row are on screen together the moment the user pauses, and in two slots they would evict each other
/// once a second. Every pause would then cost a fresh 0.5–1 s read of the footage to put back the frame
/// the click had already paid for, and every second of playback would throw away the still.
pub const FRAME_CAP: usize = 3;

struct Entry {
    handle: egui::TextureHandle,
    /// Only kept so a card can reserve the right box before its picture lands.
    width: u32,
    height: u32,
}

pub struct Cache {
    cap: usize,
    entries: HashMap<String, Entry>,
    /// Access order, oldest at the front. A `VecDeque` of keys instead of `chrono`-style timestamps
    /// because the cache is touched once per card per frame and a linear scan over 2000 keys is
    /// cheaper than maintaining a heap.
    order: VecDeque<String>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache::with_cap(CAP)
    }

    pub fn with_cap(cap: usize) -> Cache {
        Cache { cap: cap.max(1), entries: HashMap::new(), order: VecDeque::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many thumbnails are resident, shown in the title bar so the cap is visible rather than
    /// implied.
    pub fn contains(&self, key: &RowKey) -> bool {
        self.entries.contains_key(&key.texture_id())
    }

    /// Upload pixels a worker decoded. Must be called on the UI thread: it touches `Context`.
    pub fn insert(&mut self, ctx: &egui::Context, key: &RowKey, image: DecodedImage) {
        let id = key.texture_id();
        let handle = ctx.load_texture(
            id.clone(),
            egui::ColorImage::from_rgba_unmultiplied([image.width as usize, image.height as usize], &image.rgba),
            egui::TextureOptions::LINEAR,
        );
        self.entries.insert(id.clone(), Entry { handle, width: image.width, height: image.height });
        self.order.retain(|k| k != &id);
        self.order.push_back(id);
        self.evict();
    }

    /// The texture plus its natural size, touching the entry's recency.
    pub fn get(&mut self, key: &RowKey) -> Option<(egui::TextureHandle, u32, u32)> {
        let id = key.texture_id();
        let index = self.order.iter().position(|k| *k == id)?;
        let id = self.order.remove(index)?;
        self.order.push_back(id);
        self.entries.get(&key.texture_id()).map(|e| (e.handle.clone(), e.width, e.height))
    }

    /// Drop the least-recently-used entries until the cache fits.
    fn evict(&mut self) {
        while self.entries.len() > self.cap {
            match self.order.pop_front() {
                Some(id) => {
                    self.entries.remove(&id);
                }
                // Every entry in `entries` is also in `order`, so this means the cache is empty;
                // the guard keeps a bookkeeping slip from becoming an infinite loop.
                None => break,
            }
        }
    }
}

#[cfg(test)]
impl Cache {
    /// The size a resident thumbnail was decoded at, if it is here.
    pub fn size_of(&self, key: &RowKey) -> Option<(u32, u32)> {
        self.entries.get(&key.texture_id()).map(|e| (e.width, e.height))
    }
}

impl Default for Cache {
    fn default() -> Cache {
        Cache::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(rowid: i64) -> RowKey {
        RowKey::new("default_2026-09_wind.db", rowid)
    }

    fn blob() -> DecodedImage {
        DecodedImage { width: 4, height: 4, rgba: vec![9u8; 4 * 4 * 4] }
    }

    #[test]
    fn the_cache_never_grows_past_its_cap_and_frees_what_it_drops() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_cap(8);
        for i in 0..50 {
            cache.insert(&ctx, &key(i), blob());
        }
        assert_eq!(cache.len(), 8);
        assert!(!cache.contains(&key(0)), "the oldest thumbnail is gone");
        assert!(cache.contains(&key(49)));
        // Dropped handles free their texture, so the manager tracks the cache rather than the 50
        // rows that were decoded. The `+ 1` is egui's own built-in white-pixel texture.
        assert!(ctx.tex_manager().read().num_allocated() <= cache.len() + 1);
        assert!(ctx.tex_manager().read().num_allocated() < 50);
    }

    #[test]
    fn reading_an_entry_makes_it_the_most_recent() {
        let ctx = egui::Context::default();
        let mut cache = Cache::with_cap(2);
        cache.insert(&ctx, &key(1), blob());
        cache.insert(&ctx, &key(2), blob());
        assert!(cache.get(&key(1)).is_some(), "touch key 1");
        cache.insert(&ctx, &key(3), blob());
        assert!(cache.contains(&key(1)), "touched, so it survived");
        assert!(!cache.contains(&key(2)), "untouched, so it went");
    }

    #[test]
    fn an_unknown_key_is_a_miss_not_a_panic() {
        let mut cache = Cache::new();
        assert!(cache.get(&key(7)).is_none());
        assert_eq!(cache.len(), 0);
    }
}
