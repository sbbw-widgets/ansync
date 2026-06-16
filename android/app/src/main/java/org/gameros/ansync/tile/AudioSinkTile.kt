package org.gameros.ansync.tile

import android.content.Context
import android.content.Intent
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.gameros.ansync.AnsyncCompanionService
import org.gameros.ansync.PREF_AUDIO_OUT_ACTIVE
import org.gameros.ansync.PREFS

/**
 * Toggle the host→device audio route (PC audio played out of the
 * device's speaker). Symmetric to [MicShareTile] but in the other
 * direction; the service keeps a separate `AudioRouter` so both can
 * be active simultaneously without one tearing the other down.
 */
class AudioSinkTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        refresh()
    }

    override fun onClick() {
        super.onClick()
        val active = isActive()
        val intent = Intent(this, AnsyncCompanionService::class.java).apply {
            action = if (active) {
                AnsyncCompanionService.ACTION_STOP_AUDIO_SINK
            } else {
                AnsyncCompanionService.ACTION_START_AUDIO_SINK
            }
        }
        startService(intent)
        persist(!active)
        refresh()
    }

    private fun refresh() {
        val tile = qsTile ?: return
        tile.state = if (isActive()) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        tile.updateTile()
    }

    private fun isActive(): Boolean =
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getBoolean(PREF_AUDIO_OUT_ACTIVE, false)

    private fun persist(active: Boolean) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_AUDIO_OUT_ACTIVE, active)
            .apply()
    }
}
