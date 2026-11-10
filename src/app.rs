use crate::archive::{self, ArchiveWorkspace};
use crate::cli::InitialTarget;
use crate::media::{self, MediaEntry, MediaKind};
use crate::thumbnails::{ThumbSource, ThumbnailLoader};
use crate::video;
use crate::workspace::{self, LocalWorkspace};
use egui::{ColorImage, TextureHandle, TextureOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const ITEM_WIDTH: f32 = 160.0;
const THUMB_HEIGHT: f32 = 120.0;
const LABEL_HEIGHT: f32 = 22.0;
const GRID_GAP: f32 = 12.0;

/// Temp files extracted from an archive purely so an external player/browser
/// has a real path to open - not worth persisting across runs the way the
/// thumbnail cache is, so this whole directory is wiped on exit.
fn video_temp_dir() -> PathBuf {
    std::env::temp_dir().join("kotoview-video-tmp")
}

enum Workspace {
    Local(LocalWorkspace),
    Archive(ArchiveWorkspace),
}

impl Workspace {
    fn display_path(&self) -> String {
        match self {
            Workspace::Local(w) => w.root_path().to_string_lossy().into_owned(),
            Workspace::Archive(w) => w.archive_path().to_string_lossy().into_owned(),
        }
    }

    fn mode_key(&self) -> String {
        match self {
            Workspace::Local(w) => format!("local:{}", w.root_path().to_string_lossy()),
            Workspace::Archive(w) => format!("archive:{}", w.archive_path().to_string_lossy()),
        }
    }

    fn workspace_cache_key(&self) -> String {
        match self {
            Workspace::Local(w) => workspace::workspace_cache_key(&w.root_path()),
            Workspace::Archive(w) => archive::workspace_cache_key(&w.archive_path()),
        }
    }

    fn thumb_source(&self) -> ThumbSource {
        match self {
            Workspace::Local(w) => ThumbSource::Local { root: w.root_path() },
            Workspace::Archive(w) => ThumbSource::Archive {
                archive_path: w.archive_path(),
            },
        }
    }

    fn list_dir(&self, sub_path: &str) -> Result<Vec<MediaEntry>, String> {
        match self {
            Workspace::Local(w) => w.list_dir(sub_path).map_err(|e| e.to_string()),
            Workspace::Archive(w) => Ok(w.list_dir(sub_path)),
        }
    }

    fn list_flat(&self, max_depth: usize) -> Vec<MediaEntry> {
        match self {
            Workspace::Local(w) => w.list_flat(max_depth),
            Workspace::Archive(w) => w.list_flat(max_depth),
        }
    }

    fn trash_entry(&mut self, sub_path: &str) -> Result<(), String> {
        match self {
            Workspace::Local(w) => w.trash_entry(sub_path).map_err(|e| e.to_string()),
            Workspace::Archive(w) => w.trash_entry(sub_path).map_err(|e| e.to_string()),
        }
    }

    fn export_entry(&self, sub_path: &str, dest: &Path) -> Result<(), String> {
        match self {
            Workspace::Local(w) => w.export_entry(sub_path, dest).map_err(|e| e.to_string()),
            Workspace::Archive(w) => w.export_entry(sub_path, dest).map_err(|e| e.to_string()),
        }
    }

    fn read_bytes(&self, sub_path: &str) -> Result<Vec<u8>, String> {
        match self {
            Workspace::Local(w) => std::fs::read(w.root_path().join(sub_path)).map_err(|e| e.to_string()),
            Workspace::Archive(w) => w.read_entry(sub_path).map_err(|e| e.to_string()),
        }
    }

    /// A real filesystem path an external player/browser can open. Local
    /// entries already have one; archive entries have to be extracted to a
    /// throwaway temp file first (see `video_temp_dir`).
    fn video_open_path(&self, sub_path: &str) -> Result<PathBuf, String> {
        match self {
            Workspace::Local(w) => Ok(w.root_path().join(sub_path)),
            Workspace::Archive(_) => {
                let bytes = self.read_bytes(sub_path)?;
                let dir = video_temp_dir();
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let file_name = sub_path.rsplit('/').next().unwrap_or(sub_path);
                let path = dir.join(file_name);
                std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
                Ok(path)
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ViewMode {
    Browse,
    Flat,
}

struct ViewerData {
    index: usize,
    rotation: i32,
    flipped: bool,
}

enum GridAction {
    OpenDir(String),
    OpenViewerFor(String),
    OpenVideoDefault(String),
    OpenVideoBrowser(String),
}

/// Loads a single full-resolution image for the viewer overlay in the
/// background. Simpler than `ThumbnailLoader` since only one image is ever
/// "current" - no map, no concurrency cap, just a single latest-result slot.
#[derive(Clone, Default)]
struct ViewerImageLoader {
    result: Arc<Mutex<Option<(String, TextureHandle)>>>,
    loading_key: Arc<Mutex<Option<String>>>,
}

impl ViewerImageLoader {
    /// `target_size` is the display resolution (in physical pixels) the image
    /// will be shown at. Images larger than this are downsampled with a
    /// high-quality filter before upload, since GPU bilinear minification alone
    /// aliases badly on large downscale ratios (visible as jaggies, especially
    /// on Linux/wgpu where mipmapping isn't available).
    fn request(
        &self,
        ctx: &egui::Context,
        key: String,
        bytes: Result<Vec<u8>, String>,
        target_size: [u32; 2],
    ) {
        {
            let mut loading = self.loading_key.lock().unwrap();
            if loading.as_deref() == Some(key.as_str()) {
                return;
            }
            *loading = Some(key.clone());
        }
        let result = self.result.clone();
        let loading_key = self.loading_key.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Ok(bytes) = bytes {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let img = if img.width() > target_size[0] || img.height() > target_size[1] {
                        // Triangle (same filter the thumbnail generator uses)
                        // is far cheaper than Lanczos3 and still properly
                        // averages down the source, unlike GPU bilinear -
                        // Lanczos3's extra sharpness isn't worth the latency
                        // here since the result is only ever shown at this
                        // one target size (no zoom).
                        img.resize(
                            target_size[0],
                            target_size[1],
                            image::imageops::FilterType::Triangle,
                        )
                    } else {
                        img
                    };
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let handle = ctx.load_texture(key.clone(), color_image, TextureOptions::LINEAR);
                    *result.lock().unwrap() = Some((key.clone(), handle));
                }
            }
            let mut loading = loading_key.lock().unwrap();
            if loading.as_deref() == Some(key.as_str()) {
                *loading = None;
            }
            ctx.request_repaint();
        });
    }

    fn get(&self, key: &str) -> Option<TextureHandle> {
        match &*self.result.lock().unwrap() {
            Some((k, tex)) if k == key => Some(tex.clone()),
            _ => None,
        }
    }

    fn clear(&self) {
        *self.result.lock().unwrap() = None;
        *self.loading_key.lock().unwrap() = None;
    }
}

pub struct KotoViewApp {
    workspace: Option<Workspace>,
    sub_path: String,
    entries: Vec<MediaEntry>,
    view_mode: ViewMode,
    depth: u32,
    flat_entries: Vec<MediaEntry>,
    error: Option<String>,
    gen_disabled: bool,
    thumbnails: ThumbnailLoader,
    viewer: Option<ViewerData>,
    viewer_loader: ViewerImageLoader,
    pending_initial: Option<InitialTarget>,
    exit_confirmed: bool,
}

impl KotoViewApp {
    pub fn new(initial_target: Option<InitialTarget>) -> Self {
        Self {
            workspace: None,
            sub_path: String::new(),
            entries: Vec::new(),
            view_mode: ViewMode::Browse,
            depth: 3,
            flat_entries: Vec::new(),
            error: None,
            gen_disabled: false,
            thumbnails: ThumbnailLoader::new(),
            viewer: None,
            viewer_loader: ViewerImageLoader::default(),
            pending_initial: initial_target,
            exit_confirmed: false,
        }
    }

    fn media_entries(&self) -> Vec<MediaEntry> {
        let displayed = match self.view_mode {
            ViewMode::Browse => &self.entries,
            ViewMode::Flat => &self.flat_entries,
        };
        displayed
            .iter()
            .filter(|e| e.kind != MediaKind::Directory)
            .cloned()
            .collect()
    }

    fn open_root(&mut self, path: PathBuf) {
        let ws = LocalWorkspace::new(path);
        match ws.list_dir("") {
            Ok(list) => {
                self.thumbnails.clear();
                self.viewer_loader.clear();
                self.workspace = Some(Workspace::Local(ws));
                self.sub_path = String::new();
                self.entries = list;
                self.view_mode = ViewMode::Browse;
                self.viewer = None;
                self.gen_disabled = false;
                self.thumbnails.set_paused(false);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn open_archive(&mut self, path: PathBuf) {
        match ArchiveWorkspace::open(path) {
            Ok(ws) => {
                let list = ws.list_dir("");
                // zip inflate + decode is slow, so generation only defaults
                // off when there's no cache to fall back on yet - with a
                // warm cache, "generating" is really just fast disk reads.
                let has_cache =
                    media::thumbnail_cache_has_entries(&archive::workspace_cache_key(&ws.archive_path()));
                self.thumbnails.clear();
                self.viewer_loader.clear();
                self.workspace = Some(Workspace::Archive(ws));
                self.sub_path = String::new();
                self.entries = list;
                self.view_mode = ViewMode::Browse;
                self.viewer = None;
                let off = !has_cache;
                self.gen_disabled = off;
                self.thumbnails.set_paused(off);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn open_dir(&mut self, sub_path: String) {
        let Some(ws) = &self.workspace else { return };
        match ws.list_dir(&sub_path) {
            Ok(list) => {
                self.sub_path = sub_path;
                self.entries = list;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn go_up(&mut self) {
        if self.sub_path.is_empty() {
            return;
        }
        let parent = match self.sub_path.rsplit_once('/') {
            Some((p, _)) => p.to_string(),
            None => String::new(),
        };
        self.open_dir(parent);
    }

    fn refresh_flat(&mut self) {
        if let Some(ws) = &self.workspace {
            self.flat_entries = ws.list_flat(self.depth as usize);
        }
    }

    fn toggle_view_mode(&mut self) {
        if self.workspace.is_none() {
            return;
        }
        match self.view_mode {
            ViewMode::Browse => {
                self.view_mode = ViewMode::Flat;
                self.refresh_flat();
            }
            ViewMode::Flat => self.view_mode = ViewMode::Browse,
        }
    }

    fn pick_root(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.open_root(path);
        }
    }

    fn pick_archive(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("zipアーカイブを開く")
            .add_filter("zip", &["zip"])
            .pick_file()
        {
            self.open_archive(path);
        }
    }

    fn handle_deleted(&mut self, path: &str) {
        self.entries.retain(|e| e.path != path);
        self.flat_entries.retain(|e| e.path != path);
    }

    fn apply_grid_action(&mut self, action: GridAction) {
        match action {
            GridAction::OpenDir(sub_path) => self.open_dir(sub_path),
            GridAction::OpenViewerFor(sub_path) => {
                let media = self.media_entries();
                if let Some(index) = media.iter().position(|e| e.path == sub_path) {
                    self.viewer = Some(ViewerData {
                        index,
                        rotation: 0,
                        flipped: false,
                    });
                }
            }
            GridAction::OpenVideoDefault(sub_path) => self.open_video(&sub_path, false),
            GridAction::OpenVideoBrowser(sub_path) => self.open_video(&sub_path, true),
        }
    }

    fn open_video(&mut self, sub_path: &str, in_browser: bool) {
        let Some(ws) = &self.workspace else { return };
        let result = ws.video_open_path(sub_path).and_then(|path| {
            if in_browser {
                video::open_in_browser(&path)
            } else {
                video::open_with_default_app(&path)
            }
        });
        if let Err(e) = result {
            self.error = Some(e);
        }
    }

    fn show_top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("ルートフォルダを開く").clicked() {
                self.pick_root();
            }
            if ui.button("zipアーカイブを開く").clicked() {
                self.pick_archive();
            }
            if self.workspace.is_some() {
                let label = match self.view_mode {
                    ViewMode::Browse => "一覧モードへ",
                    ViewMode::Flat => "フォルダビューへ",
                };
                if ui.button(label).clicked() {
                    self.toggle_view_mode();
                }
            }
            if self.view_mode == ViewMode::Flat {
                ui.label("depth:");
                if ui
                    .add(egui::DragValue::new(&mut self.depth).range(1..=50))
                    .changed()
                {
                    self.refresh_flat();
                }
            }
            if self.view_mode == ViewMode::Browse
                && !self.sub_path.is_empty()
                && ui.button("↑ 上へ").clicked()
            {
                self.go_up();
            }
            if self.workspace.is_some() {
                let mut off = self.gen_disabled;
                if ui.checkbox(&mut off, "サムネイル生成OFF").changed() {
                    self.gen_disabled = off;
                    self.thumbnails.set_paused(off);
                }
            }
            if self.workspace.is_some() {
                let resp = ui.button("キャッシュ削除");
                egui::Popup::menu(&resp).show(|ui| {
                    if ui.button("今回の分だけ削除").clicked() {
                        if let Some(ws) = &self.workspace {
                            media::clear_thumbnail_cache_for(&ws.workspace_cache_key());
                        }
                        ui.close();
                    }
                    if ui.button("全キャッシュを削除").clicked() {
                        media::clear_thumbnail_cache();
                        ui.close();
                    }
                    if ui.button("キャンセル").clicked() {
                        ui.close();
                    }
                });
            }
        });

        if let Some(ws) = &self.workspace {
            let mut path = ws.display_path();
            if self.view_mode == ViewMode::Browse && !self.sub_path.is_empty() {
                path.push_str(" / ");
                path.push_str(&self.sub_path);
            } else if self.view_mode == ViewMode::Flat {
                path.push_str(" (一覧モード)");
            }
            ui.label(path);
        }

        if self.workspace.is_some() && !self.gen_disabled {
            let pending = self.thumbnails.pending();
            let paused = self.thumbnails.is_paused();
            if pending > 0 || paused {
                ui.horizontal(|ui| {
                    if paused {
                        ui.label("サムネイル生成を停止中(アイコン表示のまま使えます)");
                    } else {
                        ui.label(format!("サムネイル生成中… 残り{pending}件"));
                    }
                    let button_label = if paused { "再開" } else { "止める" };
                    if ui.button(button_label).clicked() {
                        self.thumbnails.set_paused(!paused);
                    }
                });
            }
        }

        if let Some(err) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(255, 99, 71), err);
        }
    }

    fn show_grid(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(workspace) = &self.workspace else { return };
        let displayed: Vec<MediaEntry> = match self.view_mode {
            ViewMode::Browse => self.entries.clone(),
            ViewMode::Flat => self.flat_entries.clone(),
        };
        let mode_key = workspace.mode_key();
        let thumb_source = workspace.thumb_source();
        let thumbnails = self.thumbnails.clone();

        let available_width = ui.available_width();
        let cell_width = ITEM_WIDTH + GRID_GAP;
        let columns = ((available_width / cell_width).floor() as usize).max(1);
        let row_height = THUMB_HEIGHT + LABEL_HEIGHT + GRID_GAP;
        let total_rows = displayed.len().div_ceil(columns);

        let mut action: Option<GridAction> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_height, total_rows, |ui, row_range| {
                for row in row_range {
                    ui.horizontal(|ui| {
                        for col in 0..columns {
                            let idx = row * columns + col;
                            let Some(entry) = displayed.get(idx) else {
                                break;
                            };
                            if let Some(a) =
                                show_cell(ui, ctx, entry, &mode_key, &thumb_source, &thumbnails)
                            {
                                action = Some(a);
                            }
                            ui.add_space(GRID_GAP);
                        }
                    });
                    ui.add_space(GRID_GAP);
                }
            });

        if let Some(action) = action {
            self.apply_grid_action(action);
        }
    }

    fn show_viewer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(workspace) = &self.workspace else {
            self.viewer = None;
            return;
        };
        let media = self.media_entries();
        let Some(viewer) = &self.viewer else { return };
        let Some(entry) = media.get(viewer.index).cloned() else {
            self.viewer = None;
            return;
        };

        let mut close = false;
        let mut deleted: Option<String> = None;
        let mut next_index: Option<usize> = None;
        let mut toggle_rotate: Option<i32> = None;
        let mut toggle_flip = false;
        let mut export_requested = false;
        let mut delete_requested = false;
        let mut play_requested = false;

        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q) {
                close = true;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                next_index = Some((viewer.index + 1) % media.len().max(1));
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                next_index = Some((viewer.index + media.len() - 1) % media.len().max(1));
            }
            if i.key_pressed(egui::Key::D) || i.key_pressed(egui::Key::Delete) {
                delete_requested = true;
            }
            if i.key_pressed(egui::Key::Enter) {
                export_requested = true;
            }
            if entry.kind == MediaKind::Video {
                if i.key_pressed(egui::Key::Space) {
                    play_requested = true;
                }
            } else if entry.kind == MediaKind::Image {
                if i.key_pressed(egui::Key::CloseBracket) {
                    toggle_rotate = Some(90);
                }
                if i.key_pressed(egui::Key::OpenBracket) {
                    toggle_rotate = Some(-90);
                }
                if i.key_pressed(egui::Key::F) {
                    toggle_flip = true;
                }
            }
        });

        let screen = ui.max_rect();
        ui.painter().rect_filled(screen, 0.0, egui::Color32::BLACK);
        ui.scope_builder(egui::UiBuilder::new().max_rect(screen), |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if ui.button("← 戻る (Esc)").clicked() {
                        close = true;
                    }
                    ui.label(&entry.name);
                    if entry.kind == MediaKind::Image {
                        if ui.button("↩ 左90度 ([)").clicked() {
                            toggle_rotate = Some(-90);
                        }
                        if ui.button("↪ 右90度 (])").clicked() {
                            toggle_rotate = Some(90);
                        }
                        if ui.button("⇋ 反転 (f)").clicked() {
                            toggle_flip = true;
                        }
                    }
                    if ui.button("💾 エクスポート (Enter)").clicked() {
                        export_requested = true;
                    }
                    if ui.button("🗑 削除 (Delete/d)").clicked() {
                        delete_requested = true;
                    }
                });

                let avail = ui.available_size();
                match entry.kind {
                    MediaKind::Video => {
                        let mode_key = workspace.mode_key();
                        let thumb_source = workspace.thumb_source();
                        let key = format!("{mode_key}::{}", entry.path);
                        self.thumbnails.request(ctx, key.clone(), thumb_source, entry.path.clone());

                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                if let Some(tex) = self.thumbnails.get(&key) {
                                    let image = egui::Image::from_texture(&tex)
                                        .max_size(avail * egui::vec2(0.8, 0.7))
                                        .shrink_to_fit();
                                    ui.add(image);
                                } else {
                                    ui.label(egui::RichText::new("🎬").size(64.0));
                                }
                                ui.add_space(8.0);
                                if ui.button(egui::RichText::new("▶ 再生する (Space)").size(18.0)).clicked() {
                                    play_requested = true;
                                }
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    if ui.button("OS既定のアプリで開く").clicked() {
                                        play_requested = true;
                                    }
                                    if ui.button("ブラウザで開く").clicked() {
                                        if let Err(e) = workspace
                                            .video_open_path(&entry.path)
                                            .and_then(|p| video::open_in_browser(&p))
                                        {
                                            self.error = Some(e);
                                        }
                                    }
                                });
                            });
                        });
                    }
                    MediaKind::Image => {
                        // Bucket the target resolution so minor window-resize
                        // jitter doesn't thrash the cache/reload, while a real
                        // resize (crossing a 64px bucket) still gets a
                        // freshly-resampled, non-jaggy texture.
                        let ppp = ctx.pixels_per_point();
                        let bucket = |v: f32| -> u32 {
                            let px = (v * ppp).ceil().max(1.0) as u32;
                            px.div_ceil(64) * 64
                        };
                        let target_size = [bucket(avail.x), bucket(avail.y)];
                        let key = format!(
                            "viewer::{}::{}::{}x{}",
                            workspace.mode_key(),
                            entry.path,
                            target_size[0],
                            target_size[1]
                        );
                        if self.viewer_loader.get(&key).is_none() {
                            let bytes = workspace.read_bytes(&entry.path);
                            self.viewer_loader.request(ctx, key.clone(), bytes, target_size);
                        }
                        if let Some(tex) = self.viewer_loader.get(&key) {
                            let mut image = egui::Image::from_texture(&tex)
                                .max_size(avail)
                                .shrink_to_fit();
                            if viewer.flipped {
                                image = image.uv(egui::Rect::from_min_max(
                                    egui::pos2(1.0, 0.0),
                                    egui::pos2(0.0, 1.0),
                                ));
                            }
                            if viewer.rotation != 0 {
                                image = image
                                    .rotate((viewer.rotation as f32).to_radians(), egui::Vec2::splat(0.5));
                            }
                            ui.centered_and_justified(|ui| ui.add(image));
                        } else {
                            ui.centered_and_justified(|ui| ui.label("読み込み中…"));
                        }
                    }
                    MediaKind::Directory => {}
                }
            });
        });

        if play_requested {
            if let Err(e) = workspace
                .video_open_path(&entry.path)
                .and_then(|p| video::open_with_default_app(&p))
            {
                self.error = Some(e);
            }
        }

        if export_requested {
            if let Some(dest) = rfd::FileDialog::new().set_file_name(&entry.name).save_file() {
                if let Err(e) = workspace.export_entry(&entry.path, &dest) {
                    self.error = Some(e);
                }
            }
        }

        if delete_requested {
            let confirmed = rfd::MessageDialog::new()
                .set_title("削除確認")
                .set_description(format!("\"{}\" を削除しますか?(.trashへ移動します)", entry.name))
                .set_buttons(rfd::MessageButtons::YesNo)
                .set_level(rfd::MessageLevel::Warning)
                .show();
            if confirmed == rfd::MessageDialogResult::Yes {
                deleted = Some(entry.path.clone());
            }
        }

        let Some(viewer) = self.viewer.as_mut() else { return };
        if let Some(delta) = toggle_rotate {
            viewer.rotation = ((viewer.rotation + delta) % 360 + 360) % 360;
        }
        if toggle_flip {
            viewer.flipped = !viewer.flipped;
        }
        if let Some(idx) = next_index {
            viewer.index = idx;
            viewer.rotation = 0;
            viewer.flipped = false;
        }
        if close {
            self.viewer = None;
        } else if let Some(path) = deleted {
            let Some(ws) = &mut self.workspace else { return };
            if let Err(e) = ws.trash_entry(&path) {
                self.error = Some(e);
            } else {
                self.handle_deleted(&path);
                let media_len = self.media_entries().len();
                if media_len == 0 {
                    self.viewer = None;
                } else if let Some(v) = self.viewer.as_mut() {
                    v.index = v.index.min(media_len - 1);
                }
            }
        }
    }
}

fn show_cell(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    entry: &MediaEntry,
    mode_key: &str,
    thumb_source: &ThumbSource,
    thumbnails: &ThumbnailLoader,
) -> Option<GridAction> {
    let mut action = None;
    ui.allocate_ui(egui::vec2(ITEM_WIDTH, THUMB_HEIGHT + LABEL_HEIGHT), |ui| {
        ui.vertical(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(ITEM_WIDTH, THUMB_HEIGHT), egui::Sense::click());
            ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
            match entry.kind {

                MediaKind::Directory => paint_icon(ui, rect, "📁"),
                MediaKind::Video | MediaKind::Image => {
                    let key = format!("{mode_key}::{}", entry.path);
                    thumbnails.request(ctx, key.clone(), thumb_source.clone(), entry.path.clone());
                    if let Some(tex) = thumbnails.get(&key) {
                        let image = egui::Image::from_texture(&tex)
                            .fit_to_exact_size(egui::vec2(ITEM_WIDTH, THUMB_HEIGHT));
                        ui.put(rect, image);
                    } else {
                        paint_icon(ui, rect, if entry.kind == MediaKind::Video { "🎬" } else { "🖼" });
                    }
                }
            }
            ui.add(
                egui::Label::new(egui::RichText::new(&entry.name).size(13.0))
                    .truncate(),
            );

            if response.clicked() {
                action = Some(match entry.kind {
                    MediaKind::Directory => GridAction::OpenDir(entry.path.clone()),
                    _ => GridAction::OpenViewerFor(entry.path.clone()),
                });
            }

            if entry.kind == MediaKind::Video {
                response.context_menu(|ui| {
                    if ui.button("OS既定のアプリで開く").clicked() {
                        action = Some(GridAction::OpenVideoDefault(entry.path.clone()));
                        ui.close();
                    }
                    if ui.button("ブラウザで開く").clicked() {
                        action = Some(GridAction::OpenVideoBrowser(entry.path.clone()));
                        ui.close();
                    }
                });
            }
        });
    });
    action
}

fn paint_icon(ui: &egui::Ui, rect: egui::Rect, icon: &str) {
    ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(32.0),
        ui.visuals().text_color(),
    );
}


impl eframe::App for KotoViewApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if let Some(target) = self.pending_initial.take() {
            match target {
                InitialTarget::Workspace(path) => self.open_root(path),
                InitialTarget::Archive(path) => self.open_archive(path),
            }
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if self.viewer.is_some() {
                self.show_viewer(ui, &ctx);
            } else {
                self.show_top_bar(ui);
                ui.separator();
                self.show_grid(ui, &ctx);
            }
        });

        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_confirmed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let delete = rfd::MessageDialog::new()
                .set_title("サムネイルキャッシュを削除しますか?")
                .set_description(
                    "削除すると次回同じフォルダ/アーカイブを開いたときにサムネイルを作り直すことになります。\n保持すると次回はキャッシュ済みの分だけ高速に表示されます。",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if delete == rfd::MessageDialogResult::Yes {
                media::clear_thumbnail_cache();
            }
            self.exit_confirmed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_exit(&mut self) {
        let _ = std::fs::remove_dir_all(video_temp_dir());
    }
}
