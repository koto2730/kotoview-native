use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Directory,
    Image,
    Video,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaEntry {
    /// '/'-separated path relative to the workspace root
    pub path: String,
    pub name: String,
    pub kind: MediaKind,
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];
const VIDEO_EXTS: &[&str] = &["mp4", "webm", "mov", "mkv", "avi"];

/// Shared by both the local-folder and zip-archive workspaces so the two stay
/// consistent about what counts as viewable media.
pub fn classify(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
pub struct ThumbnailSpec {
    pub box_size: u32,
    pub quality: u8,
}

/// Local files skip zip decompression entirely, so they can afford a
/// slightly bigger/cleaner thumbnail for the same latency budget.
pub const LOCAL_THUMBNAIL: ThumbnailSpec = ThumbnailSpec {
    box_size: 256,
    quality: 80,
};

/// Archive entries pay for zip inflate on top of the same decode+resize
/// work, so they get a smaller box and lower JPEG quality - this mostly
/// shrinks the encode/disk-write/IPC-transfer side, not the decode+inflate
/// that dominates the latency, but it's a real (if modest) win and it's
/// cheap to do.
pub const ARCHIVE_THUMBNAIL: ThumbnailSpec = ThumbnailSpec {
    box_size: 160,
    quality: 55,
};

fn thumbnail_cache_dir() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("kotoview").join("thumbnails"))
}

/// Deletes every cached thumbnail for every workspace ever opened ("全キャッシュを削除").
pub fn clear_thumbnail_cache() {
    if let Some(dir) = thumbnail_cache_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Whether any thumbnail has ever been cached to disk for this workspace -
/// used to decide whether generation should default to on or off when a
/// workspace is (re)opened: with a warm cache, "generating" is really just
/// fast disk reads, so there's no reason to default it off the way a truly
/// cold archive (all cache misses -> zip inflate + decode for every entry)
/// warrants.
pub fn thumbnail_cache_has_entries(workspace_key: &str) -> bool {
    thumbnail_cache_dir()
        .map(|d| d.join(workspace_key))
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .is_some_and(|mut entries| entries.next().is_some())
}

/// Deletes only the cached thumbnails for one workspace (one local root or
/// one archive) - the "今回の分だけ削除" case. `workspace_key` is whatever
/// `workspace::workspace_cache_key` / `archive::workspace_cache_key`
/// produced for it.
pub fn clear_thumbnail_cache_for(workspace_key: &str) {
    if let Some(dir) = thumbnail_cache_dir() {
        let _ = std::fs::remove_dir_all(dir.join(workspace_key));
    }
}

/// A stable, filesystem-safe cache key from arbitrary identifying parts
/// (source kind, path, mtime, ...). Not cryptographic - collisions would
/// just show the wrong cached thumbnail, and SipHash's 64 bits are more
/// than enough headroom for a per-user thumbnail cache.
pub fn cache_key(parts: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// JPEG is DCT-based, so the decoder can produce a 1/2, 1/4, or 1/8-size
/// image directly during IDCT instead of paying for a full-size decode that
/// then gets thrown away by the resize below. `image`'s default JPEG
/// decoder (zune-jpeg) has no such scaled-decode support, so this goes
/// through `jpeg-decoder` instead and only falls back to `image` (for
/// non-JPEG formats, or JPEG pixel formats this doesn't handle) when it
/// can't produce a result.
fn decode_jpeg_scaled(bytes: &[u8], box_size: u32) -> Option<image::DynamicImage> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    let target = box_size.min(u16::MAX as u32) as u16;
    decoder.scale(target, target).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (width, height) = (info.width as u32, info.height as u32);
    match info.pixel_format {
        PixelFormat::L8 => {
            image::GrayImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageLuma8)
        }
        PixelFormat::RGB24 => {
            image::RgbImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageRgb8)
        }
        // L16 and CMYK32 are rare enough (16-bit grayscale JPEG is
        // essentially unused; CMYK comes from some Photoshop exports) that
        // it's not worth hand-rolling a converter - fall back below instead.
        PixelFormat::L16 | PixelFormat::CMYK32 => None,
    }
}

fn make_thumbnail(bytes: &[u8], spec: ThumbnailSpec) -> image::ImageResult<Vec<u8>> {
    let img = match decode_jpeg_scaled(bytes, spec.box_size) {
        Some(img) => img,
        None => image::load_from_memory(bytes)?,
    };
    let thumb = img.resize(spec.box_size, spec.box_size, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    let encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, spec.quality);
    thumb.write_with_encoder(encoder)?;
    Ok(out)
}

/// Reads a cached thumbnail for `key` if present, otherwise calls
/// `source_bytes` (only on a cache miss - for an archive entry that means
/// decompressing it, which is the expensive part this cache exists to
/// avoid paying repeatedly) and writes the generated thumbnail back.
///
/// `workspace_key` namespaces the entry under its own subdirectory (one per
/// local root / archive) so `clear_thumbnail_cache_for` can delete just one
/// workspace's thumbnails without touching any other's.
pub fn cached_thumbnail(
    workspace_key: &str,
    key: &str,
    spec: ThumbnailSpec,
    source_bytes: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let cache_dir = thumbnail_cache_dir().map(|d| d.join(workspace_key));
    if let Some(dir) = &cache_dir {
        if let Ok(cached) = std::fs::read(dir.join(format!("{key}.jpg"))) {
            return Ok(cached);
        }
    }
    let bytes = source_bytes()?;
    let thumb = make_thumbnail(&bytes, spec).map_err(|e| e.to_string())?;
    if let Some(dir) = &cache_dir {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(format!("{key}.jpg")), &thumb);
    }
    Ok(thumb)
}
