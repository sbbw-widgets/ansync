package org.gameros.ansync

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.provider.Settings
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * One-time setup steps the user has to walk through after the cable
 * pair. We avoid a dedicated wizard Activity — instead a persistent
 * heads-up notification announces the next pending grant, the user
 * taps, the system dialog pops, the grant is recorded, and the notif
 * refreshes with the next step. No full-screen surface, no
 * launcher-icon-only navigation — everything happens in the shade.
 *
 * Each step is responsible for:
 *   * a human-readable title + body that the user sees in the notif,
 *   * an [isDone] check against either a runtime permission, a
 *     `Settings.Secure` entry, or a `SharedPreferences` flag,
 *   * a launch action handled in [SetupStepActivity].
 *
 * Optional steps (e.g. MIUI autostart) are gated by [isApplicable].
 */
enum class SetupStep(
    val key: String,
    val title: String,
    val body: String,
) {
    Notifications(
        "notifications",
        "Allow notifications",
        "Lets ansync show capture + setup updates in the status bar.",
    ),
    Microphone(
        "microphone",
        "Allow microphone",
        "Required for the PC to use this phone as a microphone.",
    ),
    Accessibility(
        "accessibility",
        "Enable Accessibility",
        "Allows the PC to send taps and keystrokes back to this device while mirroring.",
    ),
    NotificationListener(
        "notif_listener",
        "Allow Notification access",
        "Forwards your Android notifications to the desktop notification daemon.",
    ),
    MiuiAutostart(
        "miui_autostart",
        "Allow auto-start",
        "MIUI freezes background services by default. Enable Autostart + Background pop-up " +
            "so ansync stays reachable after the screen turns off.",
    ),
    ;

    fun isDone(ctx: Context): Boolean = when (this) {
        Notifications -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            ContextCompat.checkSelfPermission(
                ctx, Manifest.permission.POST_NOTIFICATIONS
            ) == PackageManager.PERMISSION_GRANTED
        } else true
        Microphone -> ContextCompat.checkSelfPermission(
            ctx, Manifest.permission.RECORD_AUDIO
        ) == PackageManager.PERMISSION_GRANTED
        Accessibility -> isAccessibilityEnabled(ctx)
        NotificationListener -> isNotifListenerEnabled(ctx)
        MiuiAutostart -> ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getBoolean(PREF_MIUI_AUTOSTART_DONE, false)
    }

    fun isApplicable(): Boolean = when (this) {
        MiuiAutostart -> isMiui()
        else -> true
    }

    companion object {
        const val PREF_MIUI_AUTOSTART_DONE = "miui_autostart_done"

        fun pending(ctx: Context): List<SetupStep> =
            values().filter { it.isApplicable() && !it.isDone(ctx) }

        fun nextPending(ctx: Context): SetupStep? = pending(ctx).firstOrNull()

        private fun isAccessibilityEnabled(ctx: Context): Boolean {
            val expected = "${ctx.packageName}/${AnsyncAccessibilityService::class.java.name}"
            val enabled = Settings.Secure.getString(
                ctx.contentResolver,
                Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
            ) ?: return false
            return enabled.split(':').any { it.equals(expected, ignoreCase = true) }
        }

        private fun isNotifListenerEnabled(ctx: Context): Boolean {
            val cn = "${ctx.packageName}/${NotificationForwarder::class.java.name}"
            val list = Settings.Secure.getString(
                ctx.contentResolver, "enabled_notification_listeners"
            ) ?: return false
            return list.split(':').any { it.equals(cn, ignoreCase = true) }
        }

        private fun isMiui(): Boolean = Build.MANUFACTURER.equals("Xiaomi", true)
            || Build.BRAND.equals("Xiaomi", true)
            || Build.BRAND.equals("Redmi", true)
            || Build.BRAND.equals("POCO", true)
    }
}

/**
 * Owns the lifecycle of the persistent "setup pending" notification.
 * Called from [AnsyncCompanionService] on every lifecycle hook that
 * could change a grant state — service onCreate, broadcast receivers
 * for app-permission changes, the SetupStepActivity result handlers.
 */
object SetupNotif {
    const val CHANNEL = "ansync.setup"
    const val NOTIFICATION_ID = 99

    /** Re-evaluate pending grants. Posts a notif if any remain, cancels otherwise. */
    fun refresh(ctx: Context) {
        ensureChannel(ctx)
        val pending = SetupStep.pending(ctx)
        val mgr = ctx.getSystemService(NotificationManager::class.java) ?: return
        val next = pending.firstOrNull()
        if (next == null) {
            mgr.cancel(NOTIFICATION_ID)
            return
        }
        val total = SetupStep.values().count { it.isApplicable() }
        val doneCount = total - pending.size
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        } else PendingIntent.FLAG_UPDATE_CURRENT
        val tapIntent = Intent(ctx, SetupStepActivity::class.java).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
            putExtra(SetupStepActivity.EXTRA_STEP_KEY, next.key)
        }
        val tapPi = PendingIntent.getActivity(ctx, next.ordinal, tapIntent, flags)
        val n = NotificationCompat.Builder(ctx, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentTitle(next.title)
            .setContentText("Step ${doneCount + 1} of $total · tap to continue")
            .setStyle(NotificationCompat.BigTextStyle().bigText(next.body))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setContentIntent(tapPi)
            .setOngoing(true)
            .setAutoCancel(false)
            .setProgress(total, doneCount, false)
            .build()
        mgr.notify(NOTIFICATION_ID, n)
    }

    private fun ensureChannel(ctx: Context) {
        val mgr = ctx.getSystemService(NotificationManager::class.java) ?: return
        if (mgr.getNotificationChannel(CHANNEL) != null) return
        val ch = NotificationChannel(
            CHANNEL,
            "ansync setup",
            NotificationManager.IMPORTANCE_HIGH,
        ).apply { description = "Guides the user through the one-time companion setup grants" }
        mgr.createNotificationChannel(ch)
    }
}
