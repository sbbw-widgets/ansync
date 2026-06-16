package org.gameros.ansync

/**
 * SharedPreferences keys shared by the service, popup activities, and
 * the QSTiles. Centralised here so any code path can read/write
 * without bouncing through an Activity that no longer exists.
 */
const val PREFS = "ansync_prefs"

/** Persisted `Uri` (as String) for the SAF tree the FS server walks. */
const val PREF_TREE_URI = "shared_tree_uri"

/** Persisted host TCP address (`host:port`) for the LAN fallback dial. */
const val PREF_HOST_ADDR = "host_addr"


/** Sticky state for the four QSTiles. Service writes + Tile reads. */
const val PREF_MIRROR_ACTIVE = "tile_mirror_active"
const val PREF_TOUCHPAD_ACTIVE = "tile_touchpad_active"
const val PREF_MIC_ACTIVE = "tile_mic_active"
const val PREF_AUDIO_OUT_ACTIVE = "tile_audio_out_active"
