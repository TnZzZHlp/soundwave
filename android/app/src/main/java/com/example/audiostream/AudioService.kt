package com.example.audiostream

import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.Process
import kotlin.math.max

/**
 * Foreground-service owner for background, lock-screen, and task-switch audio.
 * Rust owns QUIC/jitter/ring state; AudioTrack is the small Android platform shim.
 */
class AudioService : Service() {
    companion object {
        const val ACTION_CONNECT = "com.example.audiostream.action.CONNECT"
        const val ACTION_DISCONNECT = "com.example.audiostream.action.DISCONNECT"
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_FINGERPRINT = "fingerprint"
        private const val SAMPLE_RATE = 48_000
        private const val CHANNELS = 2
    }

    @Volatile private var playbackRunning = false
    @Volatile private var audioTrack: AudioTrack? = null
    private var audioThread: Thread? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var currentServer = ""
    private val mainHandler = Handler(Looper.getMainLooper())

    private val statusUpdater = object : Runnable {
        override fun run() {
            val state = NativeBridge.nativeGetState()
            val text = when (state) {
                NativeBridge.STATE_CONNECTING -> "Connecting to $currentServer"
                NativeBridge.STATE_CONNECTED -> "Connected to $currentServer"
                NativeBridge.STATE_ERROR -> "Connection error — open the app"
                NativeBridge.STATE_DISCONNECTED -> "Disconnected"
                else -> "Preparing audio stream"
            }
            getSystemService(NotificationManager::class.java)
                .notify(NotificationHelper.NOTIFICATION_ID, NotificationHelper.build(this@AudioService, text))
            if (state == NativeBridge.STATE_ERROR || state == NativeBridge.STATE_DISCONNECTED) {
                stopPlayback()
                releaseWifiLock()
            }
            if (playbackRunning || state == NativeBridge.STATE_CONNECTING || state == NativeBridge.STATE_CONNECTED) {
                mainHandler.postDelayed(this, 1_000)
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        NativeBridge.nativeInit()
        NotificationHelper.createChannel(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> {
                val host = intent.getStringExtra(EXTRA_HOST)?.trim().orEmpty()
                val port = intent.getIntExtra(EXTRA_PORT, 48_400)
                val fingerprint = intent.getStringExtra(EXTRA_FINGERPRINT)?.trim().orEmpty()
                connect(host, port, fingerprint)
            }
            ACTION_DISCONNECT -> disconnectAndStop()
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        mainHandler.removeCallbacks(statusUpdater)
        stopPlayback()
        releaseWifiLock()
        NativeBridge.nativeShutdown()
        super.onDestroy()
    }

    private fun connect(host: String, port: Int, fingerprint: String) {
        if (host.isBlank() || fingerprint.isBlank() || !NativeBridge.nativeConnect(host, port, fingerprint)) {
            currentServer = host.ifBlank { "Windows server" }
            startForeground(
                NotificationHelper.NOTIFICATION_ID,
                NotificationHelper.build(this, "Connection setup failed — open the app"),
            )
            mainHandler.removeCallbacks(statusUpdater)
            mainHandler.post(statusUpdater)
            return
        }

        currentServer = "$host:$port"
        startForeground(
            NotificationHelper.NOTIFICATION_ID,
            NotificationHelper.build(this, "Connecting to $currentServer"),
        )
        acquireWifiLock()
        startPlayback()
        mainHandler.removeCallbacks(statusUpdater)
        mainHandler.post(statusUpdater)
    }

    private fun disconnectAndStop() {
        mainHandler.removeCallbacks(statusUpdater)
        NativeBridge.nativeDisconnect()
        stopPlayback()
        releaseWifiLock()
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun startPlayback() {
        if (playbackRunning) return
        val minBufferBytes = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val bufferBytes = max(minBufferBytes, SAMPLE_RATE * CHANNELS * 2 / 25) // at least 40 ms
        val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build(),
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build(),
            )
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setBufferSizeInBytes(bufferBytes)
            .build()

        playbackRunning = true
        audioTrack = track
        audioThread = Thread({
            Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO)
            val output = ShortArray(bufferBytes / 2)
            try {
                track.play()
                while (playbackRunning) {
                    NativeBridge.nativeReadPcm(output)
                    val written = track.write(output, 0, output.size, AudioTrack.WRITE_BLOCKING)
                    if (written < 0) {
                        playbackRunning = false
                        break
                    }
                }
            } finally {
                runCatching { track.pause() }
                runCatching { track.flush() }
                track.release()
                if (audioTrack === track) audioTrack = null
            }
        }, "SoundwaveAudioTrack").apply { start() }
    }

    private fun stopPlayback() {
        playbackRunning = false
        audioTrack?.let { track ->
            runCatching { track.pause() }
            runCatching { track.flush() }
        }
        audioThread?.interrupt()
        audioThread = null
    }

    @Suppress("DEPRECATION")
    private fun acquireWifiLock() {
        releaseWifiLock()
        val manager = applicationContext.getSystemService(WIFI_SERVICE) as? WifiManager ?: return
        wifiLock = manager.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "Soundwave:streaming").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseWifiLock() {
        wifiLock?.let { lock ->
            if (lock.isHeld) lock.release()
        }
        wifiLock = null
    }
}
