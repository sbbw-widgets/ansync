package org.gameros.ansync

import android.app.Notification
import android.os.Bundle
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log

/**
 * Forwards every status-bar notification the user has granted access
 * to into the QUIC link. The host emits matching D-Bus
 * `Device.NotificationPosted` / `NotificationRemoved` signals so a
 * desktop bridge can mirror them into the user's notification
 * daemon (e.g. mako / dunst).
 *
 * Activation: the user must enable this listener under
 * Settings → Apps → Special access → Notification access. Without
 * that grant Android never binds this service.
 */
class NotificationForwarder : NotificationListenerService() {

    override fun onListenerConnected() {
        Log.i(TAG, "notification listener connected")
    }

    override fun onListenerDisconnected() {
        Log.i(TAG, "notification listener disconnected")
    }

    override fun onNotificationPosted(sbn: StatusBarNotification?) {
        sbn ?: return
        val app = sbn.packageName ?: ""
        val extras: Bundle = sbn.notification.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: ""
        val body = extras.getCharSequence(Notification.EXTRA_TEXT)?.toString() ?: ""
        try {
            NativeBridge.nativeSendNotificationPosted(sbn.id.toLong(), app, title, body)
        } catch (e: Throwable) {
            Log.w(TAG, "send notification posted threw", e)
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification?) {
        sbn ?: return
        try {
            NativeBridge.nativeSendNotificationRemoved(sbn.id.toLong())
        } catch (e: Throwable) {
            Log.w(TAG, "send notification removed threw", e)
        }
    }

    companion object {
        private const val TAG = "ansync.notif"
    }
}
