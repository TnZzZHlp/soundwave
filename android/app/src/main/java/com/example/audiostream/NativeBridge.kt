package com.example.audiostream

/** Thin JNI boundary; all streaming objects remain owned by Rust. */
object NativeBridge {
    const val STATE_IDLE = 0
    const val STATE_CONNECTING = 1
    const val STATE_CONNECTED = 2
    const val STATE_DISCONNECTED = 3
    const val STATE_ERROR = 4

    init {
        System.loadLibrary("soundwave_native")
    }

    external fun nativeConnect(host: String, port: Int, fingerprint: String): Boolean
    external fun nativeDisconnect()
    external fun nativeShutdown()
    external fun nativeGetState(): Int
    external fun nativeIsAudioActive(): Boolean
    external fun nativeWaitForUpdate(previous: Long): Long
    external fun nativeGetLatencyMs(): Long
    external fun nativeGetBufferMs(): Long
    external fun nativeGetJitterBufferMs(): Long
    external fun nativeGetBufferCapacityMs(): Long
    external fun nativeTakeVolumeAdjustment(): Int
    external fun nativeStopTestTone()
    external fun nativeGetPlaybackRequest(): Long
    external fun nativeReportPlayback(request: Long, playing: Boolean)
    external fun nativeTakeDeviceCommand(): Long
    external fun nativeReportDevice(requestId: Long, deviceId: String, name: String, values: Long)
    external fun nativeGetReceivedPackets(): Long
    external fun nativeGetLostPackets(): Long
    external fun nativeGetLatePackets(): Long
    external fun nativeGetUnderruns(): Long
    external fun nativeGetLastError(): String
    external fun nativeReadPcm(output: ShortArray): Int
}
