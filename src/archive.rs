use crate::media::{cache_key, cached_thumbnail, classify, MediaEntry, MediaKind, ARCHIVE_THUMBNAIL};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use zip::ZipArchive;

#[derive(Default, Serialize, Deserialize)]
struct TrashSidecar {
    trashed: HashSet<String>,
}

/// A zip archive opened as a viewer root. Entries are addressed by their
/// path inside the archive (as stored in the central directory, '/'
/// separated regardless of host OS).
///
/// Deletion never touches the zip itself (many archives are read-only
/// source material, e.g. downloaded manga/comic collections) — "trashing"
/// an entry just adds it to a persisted sidecar JSON next to the archive
/// (`<archive>.trash.json`) and list_dir/list_flat filter it out, mirroring
/// how LocalWorkspace hides its `.trash` folder from listings.
pub struct ArchiveWorkspace {
    archive_path: PathBuf,
    /// File entry paths inside the zip (directory entries excluded — those
    /// are inferred from path prefixes, since not every zip stores them).
    entries: Vec<String>,
    trashed: HashSet<String>,
}

impl ArchiveWorkspace {
    pub fn open(archive_path: PathBuf) -> zip::result::ZipResult<Self> {
        let file = File::open(&archive_path)?;
        let archive = ZipArchive::new(file)?;
        let entries: Vec<String> = archive
            .file_names()
            .filter(|name| !name.ends_with('/'))
            .map(|name| name.to_string())
            .collect();
        let trashed = Self::load_trash_sidecar(&archive_path);
        Ok(Self {
            archive_path,
            entries,
            trashed,
        })
    }

    fn sidecar_path(archive_path: &Path) -> PathBuf {
        let mut name = archive_path.as_os_str().to_os_string();
        name.push(".trash.json");
        PathBuf::from(name)
    }

    fn load_trash_sidecar(archive_path: &Path) -> HashSet<String> {
        std::fs::read_to_string(Self::sidecar_path(archive_path))
            .ok()
            .and_then(|s| serde_json::from_str::<TrashSidecar>(&s).ok())
            .map(|sidecar| sidecar.trashed)
            .unwrap_or_default()
    }

    fn save_trash_sidecar(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&TrashSidecar {
            trashed: self.trashed.clone(),
        })?;
        std::fs::write(Self::sidecar_path(&self.archive_path), json)
    }

    /// Immediate children of `sub_path` inside the archive — folders (inferred from
    /// path prefixes) and media files only, trashed entries excluded.
    pub fn list_dir(&self, sub_path: &str) -> Vec<MediaEntry> {
        let prefix = if sub_path.is_empty() {
            String::new()
        } else {
            format!("{sub_path}/")
        };
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        let mut files: Vec<MediaEntry> = Vec::new();
        for entry in &self.entries {
            if self.trashed.contains(entry) {
                continue;
            }
            let Some(remainder) = entry.strip_prefix(prefix.as_str()) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }
            match remainder.find('/') {
                Some(idx) => {
                    dirs.insert(remainder[..idx].to_string());
                }
                None => {
                    if let Some(kind) = classify(Path::new(remainder)) {
                        files.push(MediaEntry {
                            path: entry.clone(),
                            name: remainder.to_string(),
                            kind,
                        });
                    }
                }
            }
        }
        let mut result: Vec<MediaEntry> = dirs
            .into_iter()
            .map(|name| MediaEntry {
                path: format!("{prefix}{name}"),
                name,
                kind: MediaKind::Directory,
            })
            .collect();
        result.extend(files);
        result
    }

    /// Flat list of every image/video in the archive, up to `max_depth` directory
    /// levels deep (一覧モード). Trashed entries excluded.
    pub fn list_flat(&self, max_depth: usize) -> Vec<MediaEntry> {
        self.entries
            .iter()
            .filter(|entry| !self.trashed.contains(*entry))
            .filter(|entry| entry.matches('/').count() < max_depth)
            .filter_map(|entry| {
                classify(Path::new(entry)).map(|kind| MediaEntry {
                    path: entry.clone(),
                    name: entry.rsplit('/').next().unwrap_or(entry).to_string(),
                    kind,
                })
            })
            .collect()
    }

    pub fn trash_entry(&mut self, sub_path: &str) -> std::io::Result<()> {
        self.trashed.insert(sub_path.to_string());
        self.save_trash_sidecar()
    }

    pub fn archive_path(&self) -> PathBuf {
        self.archive_path.clone()
    }

    /// Decompresses a single entry's bytes on demand (used both to display it and to export it).
    pub fn read_entry(&self, sub_path: &str) -> zip::result::ZipResult<Vec<u8>> {
        read_zip_entry(&self.archive_path, sub_path)
    }

    pub fn export_entry(&self, sub_path: &str, dest: &Path) -> std::io::Result<()> {
        let bytes = self
            .read_entry(sub_path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(dest, bytes)
    }
}

fn read_zip_entry(archive_path: &Path, sub_path: &str) -> zip::result::ZipResult<Vec<u8>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut zip_file = archive.by_name(sub_path)?;
    let mut buf = Vec::with_capacity(zip_file.size() as usize);
    zip_file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Identifies this archive's own subdirectory in the disk thumbnail cache -
/// stable across the archive being replaced/updated (unlike the per-entry
/// key below, which does fold in mtime) so "delete this workspace's cache"
/// always finds it.
pub fn workspace_cache_key(archive_path: &Path) -> String {
    cache_key(&["archive", &archive_path.to_string_lossy()])
}

/// Free function (not `&self`) so callers - namely the `get_archive_thumbnail`
/// Tauri command - only need to hold the `AppState` lock long enough to clone
/// out the archive path, not for the whole decompress+resize+encode+disk-write
/// that follows. Without that split, every thumbnail request serialized on
/// the same mutex and a few dozen of them queued up made the app look hung
/// (window close waits for that queued work to drain).
///
/// Cache key includes the archive's own mtime, so replacing the zip
/// invalidates thumbnails for entries whose bytes changed.
pub fn thumbnail_for_path(archive_path: &Path, sub_path: &str) -> Result<Vec<u8>, String> {
    let archive_mtime = std::fs::metadata(archive_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = cache_key(&[&archive_mtime.to_string(), sub_path]);
    cached_thumbnail(&workspace_cache_key(archive_path), &key, ARCHIVE_THUMBNAIL, || {
        let path = Path::new(sub_path);
        if classify(path) == Some(MediaKind::Video) {
            let bytes = read_zip_entry(archive_path, sub_path).map_err(|e| e.to_string())?;
            let temp_dir = std::env::temp_dir().join("kotoview-video-tmp");
            let _ = std::fs::create_dir_all(&temp_dir);
            let file_name = sub_path.rsplit('/').next().unwrap_or(sub_path);
            let temp_path = temp_dir.join(format!("thumb_{file_name}"));
            let _ = std::fs::write(&temp_path, &bytes);
            let result = crate::video::extract_frame_bytes(&temp_path);
            let _ = std::fs::remove_file(&temp_path);
            result
        } else {
            read_zip_entry(archive_path, sub_path).map_err(|e| e.to_string())
        }
    })
}

