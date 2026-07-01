package org.gameros.ansync.tile

import android.content.Context
import android.content.Intent
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.gameros.ansync.AnsyncCompanionService
import org.gameros.ansync.PREF_CAMERA_ACTIVE
import org.gameros.ansync.PREFS

/**
 * Toggle the phone → PC camera stream.
 *
 * Short tap: start with the last-saved [org.gameros.ansync.CameraLocalConfig]
 * (or defaults on first run) via
 * [AnsyncCompanionService.ACTION_START_CAMERA] / [AnsyncCompanionService.ACTION_STOP_CAMERA].
 *
 * Long press: Android routes to [org.gameros.ansync.CameraSettingsActivity]
 * (declared via `QS_TILE_PREFERENCES` in the manifest) so the user
 * can edit resolution / fps / codec before starting.
 */
class CameraTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        refresh()
    }

    override fun onClick() {
        super.onClick()
        val active = isActive()
        val action = if (active) {
            AnsyncCompanionService.ACTION_STOP_CAMERA
        } else {
            AnsyncCompanionService.ACTION_START_CAMERA
        }
        val intent = Intent(this, AnsyncCompanionService::class.java).setAction(action)
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
            .getBoolean(PREF_CAMERA_ACTIVE, false)

    private fun persist(active: Boolean) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_CAMERA_ACTIVE, active)
            .apply()
    }
}
