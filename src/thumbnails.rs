use crate::{archive, workspace};
use egui::{ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Bounded so a session spent scrolling through tens of thousands of images
/// doesn't just keep growing GPU memory forever - oldest textures get freed
/// (dropping the last `TextureHandle` clone frees the texture) once the
/// cache is over the cap. Mirrors thumbnailCache.ts's `MAX_CACHED`.
const MAX_CACHED: usize = 800;

#[derive(Clone)]
pub enum ThumbSource {
    Local { root: PathBuf },
    Archive { archive_path: PathBuf },
}

/// Loads grid thumbnails on background threads and caches them as egui
/// textures, keyed by `"{mode_key}::{entry.path}"`.
///
/// Unlike the original TS version (`thumbnailCache.ts`), there is no
/// explicit queue: egui's immediate-mode model means `request()` is only
/// called once per frame for on-screen cells (via `ScrollArea::show_rows`),
/// so "wanting" a thumbnail is naturally re-issued every frame a cell is
/// visible and undispatched, and the concurrency cap just makes `request()`
/// a no-op until a slot frees up. This sidesteps the whole class of bugs the
/// TS version had to work around (stuck promises, StrictMode double-mount,
/// orphaned in-flight entries on pause) - see kotoview's work/HANDOFF.md.
#[derive(Clone)]
pub struct ThumbnailLoader {
    cache: Arc<Mutex<HashMap<String, TextureHandle>>>,
    order: Arc<Mutex<VecDeque<String>>>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    active: Arc<AtomicUsize>,
    paused: Arc<AtomicBool>,
    max_concurrent: usize,
}

impl ThumbnailLoader {
    pub fn new() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            order: Arc::new(Mutex::new(VecDeque::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            active: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
            max_concurrent: cores.clamp(2, 6),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, value: bool) {
        self.paused.store(value, Ordering::Relaxed);
    }

    /// Approximate "still working" count for a progress readout - counts
    /// only dispatched-but-not-finished requests, not cells that merely
    /// want one but are waiting for a free concurrency slot.
    pub fn pending(&self) -> usize {
        self.in_flight.lock().unwrap().len()
    }

    pub fn get(&self, key: &str) -> Option<TextureHandle> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    /// Clears every cached texture - called when switching workspace so the
    /// old workspace's thumbnails don't linger in GPU memory.
    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
        self.order.lock().unwrap().clear();
        self.in_flight.lock().unwrap().clear();
    }

    /// Call once per visible grid cell each frame. No-ops if the thumbnail
    /// is already cached, already in flight, generation is paused, or the
    /// concurrency cap is reached - callers don't need to guard any of that
    /// themselves.
    pub fn request(&self, ctx: &egui::Context, key: String, source: ThumbSource, sub_path: String) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        if self.cache.lock().unwrap().contains_key(&key) {
            return;
        }
        {
            let mut in_flight = self.in_flight.lock().unwrap();
            if in_flight.contains(&key) {
                return;
            }
            if self.active.load(Ordering::Relaxed) >= self.max_concurrent {
                return;
            }
            in_flight.insert(key.clone());
        }
        self.active.fetch_add(1, Ordering::Relaxed);

        let cache = self.cache.clone();
        let order = self.order.clone();
        let in_flight = self.in_flight.clone();
        let active = self.active.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let bytes = match &source {
                ThumbSource::Local { root } => workspace::thumbnail_for_root(root, &sub_path),
                ThumbSource::Archive { archive_path } => {
                    archive::thumbnail_for_path(archive_path, &sub_path)
                }
            };
            if let Ok(bytes) = bytes {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let handle = ctx.load_texture(key.clone(), color_image, TextureOptions::LINEAR);
                    let mut cache = cache.lock().unwrap();
                    cache.insert(key.clone(), handle);
                    let mut order = order.lock().unwrap();
                    order.push_back(key.clone());
                    while order.len() > MAX_CACHED {
                        if let Some(oldest) = order.pop_front() {
                            cache.remove(&oldest);
                        }
                    }
                }
            }
            in_flight.lock().unwrap().remove(&key);
            active.fetch_sub(1, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }
}
