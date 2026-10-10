//! File drops on a native Wayland window. winit 0.30 has no Wayland drag and drop, so a file
//! dragged from a file manager onto PhotoCraft does nothing there (#386). This crate joins
//! winit's own Wayland connection on a separate event queue, binds `wl_data_device`, accepts
//! `text/uri-list` drags over our window and reports where the pointer is while they hover and
//! which paths were dropped where.
//!
//! [`Listener::spawn`] starts the worker thread; dropping the [`Listener`] stops and joins it.
//! Positions are surface-local logical pixels, the window's own coordinate space.
//!
//! The only `unsafe` is joining winit's connection (see `linux.rs`); the URI parsing is plain
//! data and tested on every platform.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_os = "linux")]
mod linux;
mod uri_list;

#[cfg(target_os = "linux")]
pub use linux::Listener;
pub use uri_list::parse_uri_list;

use std::path::PathBuf;

/// Files dropped onto the window.
#[derive(Clone, Debug, PartialEq)]
pub struct Dropped {
    /// Absolute local paths, in the order the source listed them.
    pub paths: Vec<PathBuf>,
    /// Where they were dropped, in surface-local logical pixels.
    pub at: (f64, f64),
}
