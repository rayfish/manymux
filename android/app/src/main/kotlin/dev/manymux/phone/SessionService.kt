package dev.manymux.phone

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import uniffi.manymux_android.State

/** Keep SSH and its screen reader running while another app is on screen. */
class SessionService : Service() {
    private val here = Handler(Looper.getMainLooper())
    private val client get() = application as ManymuxApplication
    private var shown: String? = null
    private var owned: ManymuxApplication.OpenSession? = null

    private val tick = object : Runnable {
        override fun run() {
            val session = client.session
            val detail = when (session?.attach?.state()) {
                is State.Reaching -> "Connecting"
                is State.Attached -> "Connected"
                is State.Waiting -> "Reconnecting"
                else -> {
                    stopForeground(STOP_FOREGROUND_REMOVE)
                    stopSelf()
                    return
                }
            }
            val text = "${session.machine.address}/${session.name}: $detail"
            if (shown != text) {
                foreground(notification(text))
                shown = text
            }
            here.postDelayed(this, 1000)
        }
    }

    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(CHANNEL, "Open session", NotificationManager.IMPORTANCE_LOW).apply {
                    description = "Keeps an open terminal connected while you use other apps"
                },
            )
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        owned = client.session
        foreground(notification("Keeping your terminal connected"))
        shown = null
        here.removeCallbacks(tick)
        tick.run()
        return START_NOT_STICKY
    }

    private fun foreground(notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIFICATION, notification)
        }
    }

    private fun notification(text: String): Notification {
        val returnToSession = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(this, CHANNEL)
        else Notification.Builder(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
        }
        return builder
            .setSmallIcon(R.drawable.ic_session)
            .setContentTitle("manymux")
            .setContentText(text)
            .setContentIntent(returnToSession)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .setVisibility(Notification.VISIBILITY_SECRET)
            .build()
    }

    override fun onDestroy() {
        here.removeCallbacks(tick)
        // A system stop leaves the handle readable by an activity still showing it.
        client.session?.takeIf { it === owned }?.attach?.detach()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL = "open-session"
        private const val NOTIFICATION = 1
    }
}
