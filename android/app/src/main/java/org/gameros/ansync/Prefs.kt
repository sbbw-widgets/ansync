package org.gameros.ansync

/**
 * SharedPreferences keys shared by the service, popup activities, and
 * the QSTiles. Centralised here so any code path can read/write
 * without bouncing through an Activity that no longer exists.
 */
const val PREFS = "ansync_prefs"

/** Persisted host TCP address (`host:port`) for the LAN fallback dial. */
const val PREF_HOST_ADDR = "host_addr"


/** Sticky state for the QSTiles. Service writes + Tile reads. */
const val PREF_MIRROR_ACTIVE = "tile_mirror_active"
const val PREF_TOUCHPAD_ACTIVE = "tile_touchpad_active"
const val PREF_MIC_ACTIVE = "tile_mic_active"

/**
 * Opt-in: hold a `PARTIAL_WAKE_LOCK` while at least one stream
 * (mirror, camera, audio in/out) is active. Off by default; battery
 * cost ~5%/h on the device. Flip via the
 * `org.gameros.ansync.action.SET_CPU_WAKE_LOCK` broadcast with
 * boolean extra `enabled`.
 */
const val PREF_CPU_WAKE_LOCK = "cpu_wake_lock"
