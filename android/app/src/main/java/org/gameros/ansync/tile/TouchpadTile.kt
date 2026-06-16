package org.gameros.ansync.tile

import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.gameros.ansync.TouchpadActivity

/**
 * Tap → open the full-screen [TouchpadActivity]. The activity itself
 * is the overlay that captures MotionEvents and pushes them to the
 * host. There is no on/off state — pressing back inside the activity
 * dismisses it, so we keep the tile in `STATE_INACTIVE` (acts like a
 * stateless shortcut button in the QS shade).
 */
class TouchpadTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        qsTile?.state = Tile.STATE_INACTIVE
        qsTile?.updateTile()
    }

    override fun onClick() {
        super.onClick()
        val intent = Intent(this, TouchpadActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            val pi = PendingIntent.getActivity(
                this, 0, intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            startActivityAndCollapse(pi)
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
