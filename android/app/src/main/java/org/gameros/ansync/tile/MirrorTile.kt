package org.gameros.ansync.tile

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.gameros.ansync.AnsyncCompanionService
import org.gameros.ansync.GrantScreenCaptureActivity
import org.gameros.ansync.PREF_MIRROR_ACTIVE
import org.gameros.ansync.PREFS

/**
 * Quick Settings tile that toggles MediaProjection mirror to the
 * paired host. Off → tap launches [GrantScreenCaptureActivity] which
 * pops the system picker; on RESULT_OK the service starts the
 * capture session and writes [PREF_MIRROR_ACTIVE]. On → tap sends
 * [AnsyncCompanionService.ACTION_STOP_CAPTURE].
 *
 * MediaProjection consent is per-session per Android policy, so a
 * fresh picker pops every time the tile flips on, even if the user
 * granted it during the previous session.
 */
class MirrorTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        refresh()
    }

    override fun onClick() {
        super.onClick()
        val active = isActive()
        if (active) {
            val stop = Intent(this, AnsyncCompanionService::class.java).apply {
                action = AnsyncCompanionService.ACTION_STOP_CAPTURE
            }
            startService(stop)
            persist(false)
        } else {
            val grant = Intent(this, GrantScreenCaptureActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                val pi = PendingIntent.getActivity(
                    this, 0, grant,
                    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                )
                startActivityAndCollapse(pi)
            } else {
                @Suppress("DEPRECATION")
                startActivityAndCollapse(grant)
            }
        }
        refresh()
    }

    private fun refresh() {
        val tile = qsTile ?: return
        tile.state = if (isActive()) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        tile.updateTile()
    }

    private fun isActive(): Boolean =
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getBoolean(PREF_MIRROR_ACTIVE, false)

    private fun persist(active: Boolean) {
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_MIRROR_ACTIVE, active)
            .apply()
    }
}
