package com.example.audiostream

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon

object NotificationHelper {
    const val CHANNEL_ID = "audio_stream_playback"
    const val NOTIFICATION_ID = 48400

    fun createChannel(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                context.getString(R.string.notification_channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps LAN audio streaming active in the background"
            },
        )
    }

    fun build(context: Context, connectionText: String): Notification {
        val disconnectIntent = Intent(context, AudioService::class.java).apply {
            action = AudioService.ACTION_DISCONNECT
        }
        val disconnectPendingIntent = PendingIntent.getService(
            context,
            1,
            disconnectIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val action = Notification.Action.Builder(
            Icon.createWithResource(context, android.R.drawable.ic_menu_close_clear_cancel),
            "Disconnect",
            disconnectPendingIntent,
        ).build()

        return Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_headset)
            .setContentTitle("Audio Stream")
            .setContentText(connectionText)
            .setOngoing(true)
            .addAction(action)
            .build()
    }
}
