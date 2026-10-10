//! File drops on native Wayland (#386), which winit 0.30 doesn't deliver: the
//! `photocraft-wayland-drop` listener's drags and drops become egui's hovered and dropped files,
//! and its drag position is the pointer the drop is routed by (canvas, tab strip or panels; see
//! `file_open::DropTarget`), as `cursor.rs` reads it on the other platforms.
//!
//! Local patch: upstream PhotoCraft waits for egui to move to winit 0.31, which has Wayland drag
//! and drop.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use eframe::wgpu::rwh::{HasDisplayHandle, HasWindowHandle};
use photocraft_ui_egui::{CursorPosFn, NativeDropsFn};
use photocraft_wayland_drop::Listener;

/// The drop and pointer services for a native Wayland window; `None` on X11 or when the
/// compositor offers no drag and drop.
pub fn services(cc: &eframe::CreationContext<'_>) -> Option<(NativeDropsFn, CursorPosFn)> {
    let ctx = cc.egui_ctx.clone();
    let listener = Rc::new(Listener::spawn(cc.display_handle().ok()?, cc.window_handle().ok()?, move || ctx.request_repaint())?);
    let drops = listener.clone();
    let native_drops: NativeDropsFn = Box::new(move |raw: &mut egui::RawInput| {
        // A Wayland drag names its files only on drop: hover with no path, as egui's web shell does.
        if drops.hovering() && raw.hovered_files.is_empty() {
            raw.hovered_files.push(egui::HoveredFile::default());
        }
        for dropped in drops.take_drops() {
            raw.dropped_files.extend(dropped.paths.into_iter().map(|path| Arc::new(PathDrop(path)) as egui::DroppedFileHandle));
        }
    });
    // Surface-local logical pixels are egui points before zoom.
    let cursor_pos: CursorPosFn = Box::new(move |ctx: &egui::Context| listener.pointer().map(|(x, y)| egui::pos2(x as f32, y as f32) / ctx.zoom_factor()));
    Some((native_drops, cursor_pos))
}

/// A dropped local file. The app opens absolute paths itself, in bounded reads (#375).
#[derive(Debug)]
struct PathDrop(PathBuf);

impl egui::DroppedFile for PathDrop {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}
