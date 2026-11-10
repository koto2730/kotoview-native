use crate::media::{cache_key, cached_thumbnail, classify, MediaEntry, MediaKind, LOCAL_THUMBNAIL};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A local folder opened as a viewer root. Archive roots (zip) are handled by a
/// separate, parallel set of commands rather than a shared trait — root mode and archive
/// mode are mutually exclusive, and this mirrors kotoshelf's local/ssh command-pair pattern.
pub struct LocalWorkspace {
    pub root: PathBuf,
}

impl LocalWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Immediate children of `sub_path` (relative to root) — folders and media files only.
    /// Used for grid navigation (select a folder to descend, ESC to go up).
    pub fn list_dir(&self, sub_path: &str) -> std::io::Result<Vec<MediaEntry>> {
        let dir = self.root.join(sub_path);
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".trash" {
                continue;
            }
            let kind = if path.is_dir() {
                MediaKind::Directory
            } else {
                match classify(&path) {
                    Some(k) => k,
                    None => continue,
                }
            };
            entries.push(MediaEntry {
                path: relative_slash_path(&self.root, &path),
                name,
                kind,
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != MediaKind::Directory)
                .cmp(&(b.kind != MediaKind::Directory))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    /// Flat list of every image/video under the root, up to `max_depth` levels deep
    /// (一覧モード). `.trash` is never descended into.
    pub fn list_flat(&self, max_depth: usize) -> Vec<MediaEntry> {
        WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".trash")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let kind = classify(e.path())?;
                Some(MediaEntry {
                    path: relative_slash_path(&self.root, e.path()),
                    name: e.file_name().to_string_lossy().into_owned(),
                    kind,
                })
            })
            .collect()
    }

    /// Moves `sub_path` under `<root>/.trash/`, preserving its relative sub-directory
    /// structure so restoring is a plain move back.
    pub fn trash_entry(&self, sub_path: &str) -> std::io::Result<()> {
        let src = self.root.join(sub_path);
        let dest = self.root.join(".trash").join(sub_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(src, dest)
    }

    /// Copies `sub_path` out to an arbitrary destination outside the root.
    pub fn export_entry(&self, sub_path: &str, dest: &Path) -> std::io::Result<()> {
        std::fs::copy(self.root.join(sub_path), dest)?;
        Ok(())
    }

    pub fn root_path(&self) -> PathBuf {
        self.root.clone()
    }
}

/// Identifies this root's own subdirectory in the disk thumbnail cache -
/// stable across file edits (unlike the per-entry key below) so "delete this
/// workspace's cache" always finds it regardless of what changed inside.
pub fn workspace_cache_key(root: &Path) -> String {
    cache_key(&["local", &root.to_string_lossy()])
}

/// Free function (not `&self`) for the same reason as `archive::thumbnail_for_path`:
/// callers should only hold the `AppState` lock long enough to clone the root
/// path, not for the whole decompress+resize+encode+disk-write that follows.
/// Cache key includes mtime + size so edits to the source file invalidate it.
pub fn thumbnail_for_root(root: &Path, sub_path: &str) -> Result<Vec<u8>, String> {
    let path = root.join(sub_path);
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = cache_key(&[
        &path.to_string_lossy(),
        &mtime.to_string(),
        &meta.len().to_string(),
    ]);
    cached_thumbnail(&workspace_cache_key(root), &key, LOCAL_THUMBNAIL, || {
        if classify(&path) == Some(MediaKind::Video) {
            crate::video::extract_frame_bytes(&path)
        } else {
            std::fs::read(&path).map_err(|e| e.to_string())
        }
    })
}

