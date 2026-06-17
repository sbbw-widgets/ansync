//! `ansyncd` library surface.
//!
//! Renderer for live peer mirroring moved to a subprocess in
//! `mirror_renderer` — one process per open window, so winit's
//! once-per-process `EventLoop::build` guard never blocks the user
//! from closing + reopening a mirror. The dev-only Annex-B file
//! feeder lives in `mirror_window`.

pub mod mirror_renderer;
pub mod mirror_window;
