package org.gameros.ansync.tile

import android.content.Context
import android.content.Intent
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.gameros.ansync.AnsyncCompanionService
import org.gameros.ansync.PREF_MIC_ACTIVE
import org.gameros.ansync.PREFS

/**
 * Toggle the device→host audio route (mic forwarding). On click we
 * fire an Intent that the service translates into an `AudioRouter`
 * lifecycle change. Live state is mirrored in
 * [PREF_MIC_ACTIVE] so the tile renders the right colour on the next
 * `onStartListening` regardless of whether the service is bound.
 */
class MicShareTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        refresh()
    }

    override fun onClick() {
        super.onClick()
        val active = isActive()
        val intent = Intent(this, AnsyncCompanionService::class.java).apply {
            action = if (active) {
                AnsyncCompanionService.ACTION_STOP_MIC_SHARE
            } else {
                AnsyncCompanionService.ACTION_START_MIC_SHARE
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
            .getBoolean(PREF_MIC_ACTIVE, false)

    private fun persist(active: Boolean) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_MIC_ACTIVE, active)
            .apply()
    }
}
