//! `ansyncd` library surface.
//!
//! Hosts the renderer that the daemon binary drives once a D-Bus
//! `ShowScreen` call (or `--play-file` in dev builds) supplies a
//! [`mirror_window::LatestFrame`]. Sits as a library next to the bin
//! so the renderer's public items (`MirrorApp`, conversion helpers,
//! `run`) are part of an exported surface — Step 6 closes before the
//! prod caller lands in Step 7, and bins do not get the "pub items
//! are an exported API" exemption from the `dead_code` lint.

pub mod mirror_window;
