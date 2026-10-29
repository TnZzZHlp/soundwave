package com.example.audiostream

import android.content.Context
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings

internal class DeviceVolumeControl(context: Context) {
    private val manager = context.getSystemService(AudioManager::class.java)
    private val deviceId = Settings.Secure.getString(context.contentResolver, Settings.Secure.ANDROID_ID) ?: ""
    private val name = "${Build.MANUFACTURER} ${Build.MODEL}".take(80)
    private val handler = Handler(Looper.getMainLooper())
    private var originalVolume: Int? = null
    private var deadline = 0L
    private val watchdog = object : Runnable {
        override fun run() {
            if (originalVolume != null && (SystemClock.elapsedRealtime() >= deadline || NativeBridge.nativeGetState() != NativeBridge.STATE_CONNECTED)) {
                report(0, !cancelCalibration())
            }
            handler.postDelayed(this, 500)
        }
    }
    private val routeCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(devices: Array<out AudioDeviceInfo>) = routeChanged()
        override fun onAudioDevicesRemoved(devices: Array<out AudioDeviceInfo>) = routeChanged()
    }

    init {
        manager.registerAudioDeviceCallback(routeCallback, handler)
        handler.postDelayed(watchdog, 500)
    }

    fun close() {
        cancelCalibration()
        handler.removeCallbacks(watchdog)
        manager.unregisterAudioDeviceCallback(routeCallback)
    }

    fun cancelCalibration(): Boolean {
        NativeBridge.nativeStopTestTone()
        val original = originalVolume ?: return true
        originalVolume = null
        return runCatching {
            if (!speakerOnly()) return@runCatching false
            manager.setStreamVolume(AudioManager.STREAM_MUSIC, original, 0)
            manager.getStreamVolume(AudioManager.STREAM_MUSIC) == original
        }.getOrDefault(false)
    }

    private fun routeChanged() {
        report(0, !cancelCalibration())
    }

    fun applyPending() {
        while (true) {
            val command = NativeBridge.nativeTakeDeviceCommand()
            if (command == 0L) return
            val requestId = command ushr 24
            val operation = (command ushr 16) and 0xff
            var failed = false
            try {
                if (originalVolume != null && SystemClock.elapsedRealtime() >= deadline) {
                    failed = !cancelCalibration()
                }
                when (operation) {
                    1L -> if (originalVolume != null) deadline = SystemClock.elapsedRealtime() + 6000
                    2L, 6L, 7L -> {
                        if (manager.isVolumeFixed || (operation != 2L && !speakerOnly()) || (operation == 6L && originalVolume == null) || (originalVolume != null && !speakerOnly())) {
                            failed = true
                        } else {
                            val index = (command and 0xffff).toInt().coerceIn(minimum(), manager.getStreamMaxVolume(AudioManager.STREAM_MUSIC))
                            manager.setStreamVolume(AudioManager.STREAM_MUSIC, index, 0)
                            failed = manager.getStreamVolume(AudioManager.STREAM_MUSIC) != index
                        }
                    }
                    3L -> {
                        if (manager.isVolumeFixed || !speakerOnly()) {
                            failed = true
                        } else {
                            if (originalVolume == null) originalVolume = manager.getStreamVolume(AudioManager.STREAM_MUSIC)
                            deadline = SystemClock.elapsedRealtime() + 6000
                        }
                    }
                    4L -> failed = !cancelCalibration()
                    5L -> {
                        val expected = (command and 0xffff).toInt()
                        if (originalVolume == null || !speakerOnly() || manager.getStreamVolume(AudioManager.STREAM_MUSIC) != expected) {
                            failed = true
                        } else {
                            originalVolume = null
                        }
                    }
                }
            } catch (_: RuntimeException) {
                failed = true
            }
            report(requestId, failed)
        }
    }

    private fun minimum(): Int = if (Build.VERSION.SDK_INT >= 28) manager.getStreamMinVolume(AudioManager.STREAM_MUSIC) else 0

    private fun speakerOnly(): Boolean = manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).all {
        it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER || it.type == AudioDeviceInfo.TYPE_BUILTIN_EARPIECE
    }

    private fun report(requestId: Long, failed: Boolean) {
        try {
            var values = manager.getStreamVolume(AudioManager.STREAM_MUSIC).toLong()
            values = values or (minimum().toLong() shl 16)
            values = values or (manager.getStreamMaxVolume(AudioManager.STREAM_MUSIC).toLong() shl 32)
            if (manager.isVolumeFixed) values = values or (1L shl 48)
            if (speakerOnly()) values = values or (1L shl 49)
            if (failed) values = values or (1L shl 50)
            NativeBridge.nativeReportDevice(requestId, deviceId, name, values)
        } catch (_: RuntimeException) {
            NativeBridge.nativeReportDevice(requestId, deviceId, name, (1L shl 48) or (1L shl 50))
        }
    }
}
