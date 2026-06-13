//! `ansyncd` library surface.
//!
//! Now thin: renderer moved to `ansync_video::sink_egui` so
//! daemon-core can drive the same window from `Device.ShowScreen`.
//! Only the dev-only Annex-B file feeder lives here.

pub mod mirror_window;
