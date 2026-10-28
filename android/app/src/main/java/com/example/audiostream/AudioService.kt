package com.example.audiostream

import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import kotlin.math.abs
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
        private const val PLAYBACK_CHUNK_FRAMES = SAMPLE_RATE / 200
        private const val AUDIO_BUFFER_FRAMES = PLAYBACK_CHUNK_FRAMES * 2
        private const val FRAME_SAMPLES = PLAYBACK_CHUNK_FRAMES * CHANNELS
    }

    @Volatile private var playbackRunning = false
    @Volatile private var audioTrack: AudioTrack? = null
    @Volatile private var audioThread: Thread? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var reconnectRequested = false
    private var currentServer = ""
    private var lastNotifiedState = Int.MIN_VALUE
    private val mainHandler = Handler(Looper.getMainLooper())

    @Volatile private var statusThread: Thread? = null

    private fun updateStatus() {
        val state = NativeBridge.nativeGetState()
        val text = when (state) {
            NativeBridge.STATE_CONNECTING -> "Connecting to $currentServer"
            NativeBridge.STATE_CONNECTED -> "Connected to $currentServer"
            NativeBridge.STATE_ERROR -> "Connection lost — retrying"
            NativeBridge.STATE_DISCONNECTED -> "Disconnected"
            else -> "Preparing audio stream"
        }
        if (state != lastNotifiedState) {
            getSystemService(NotificationManager::class.java)
                .notify(NotificationHelper.NOTIFICATION_ID, NotificationHelper.build(this@AudioService, text))
            lastNotifiedState = state
        }
        syncStreamingResources(state)
    }

    private fun watchStatus() {
        if (statusThread != null) return
        val thread = Thread({
            val watcher = Thread.currentThread()
            var revision = -1L
            while (statusThread === watcher) {
                val updated = NativeBridge.nativeWaitForUpdate(revision)
                if (updated != revision) {
                    revision = updated
                    mainHandler.post {
                        if (statusThread === watcher) updateStatus()
                    }
                }
            }
        }, "SoundwaveStatus")
        statusThread = thread
        thread.start()
    }

    override fun onCreate() {
        super.onCreate()
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
        reconnectRequested = false
        lastNotifiedState = Int.MIN_VALUE
        statusThread = null
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
            updateStatus()
            return
        }

        reconnectRequested = true
        lastNotifiedState = Int.MIN_VALUE
        currentServer = "$host:$port"
        startForeground(
            NotificationHelper.NOTIFICATION_ID,
            NotificationHelper.build(this, "Connecting to $currentServer"),
        )
        syncStreamingResources(NativeBridge.nativeGetState())
        watchStatus()
    }

    private fun disconnectAndStop() {
        reconnectRequested = false
        lastNotifiedState = Int.MIN_VALUE
        statusThread = null
        NativeBridge.nativeDisconnect()
        stopPlayback()
        releaseWifiLock()
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun syncStreamingResources(state: Int) {
        if (reconnectRequested && state == NativeBridge.STATE_CONNECTED) {
            applyVolumeAdjustment()
        }
        if (reconnectRequested && state == NativeBridge.STATE_CONNECTED && NativeBridge.nativeIsAudioActive()) {
            acquireWifiLock()
            startPlayback()
        } else {
            stopPlayback()
            releaseWifiLock()
        }
    }

    private fun startPlayback() {
        // Keep ownership until the old worker releases its track, so a fast
        // reconnect cannot restart a worker that was asked to stop.
        if (audioTrack != null) return
        val minBufferBytes = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val bufferBytes = max(minBufferBytes, AUDIO_BUFFER_FRAMES * CHANNELS * 2)
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
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .setBufferSizeInBytes(bufferBytes)
            .build()
        // getMinBufferSize() is a safe capacity, not a latency target. Shrink
        // the effective queue after construction; Android permits this for a
        // low-latency track and keeps the larger allocation as a safety margin.
        track.setBufferSizeInFrames(AUDIO_BUFFER_FRAMES)

        playbackRunning = true
        audioTrack = track
        val thread = Thread({
            try {
                AudioTrackPlayback.run(track, FRAME_SAMPLES) {
                    playbackRunning && audioTrack === track
                }
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
            } finally {
                runCatching { track.pause() }
                runCatching { track.flush() }
                track.release()
                if (audioTrack === track) {
                    playbackRunning = false
                    audioTrack = null
                }
                if (audioThread === Thread.currentThread()) audioThread = null
                mainHandler.post {
                    if (reconnectRequested) syncStreamingResources(NativeBridge.nativeGetState())
                }
            }
        }, "SoundwaveAudioTrack")
        audioThread = thread
        thread.start()
    }

    private fun applyVolumeAdjustment() {
        val steps = NativeBridge.nativeTakeVolumeAdjustment()
        if (steps == 0) return
        val manager = getSystemService(AudioManager::class.java)
        val direction = if (steps > 0) AudioManager.ADJUST_RAISE else AudioManager.ADJUST_LOWER
        runCatching {
            repeat(abs(steps)) {
                manager.adjustStreamVolume(AudioManager.STREAM_MUSIC, direction, 0)
            }
        }
    }

    private fun stopPlayback() {
        if (!playbackRunning) return
        playbackRunning = false
        audioTrack?.let { track ->
            runCatching { track.pause() }
            runCatching { track.flush() }
        }
        audioThread?.interrupt()
    }

    @Suppress("DEPRECATION")
    private fun acquireWifiLock() {
        if (wifiLock?.isHeld == true) return
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
