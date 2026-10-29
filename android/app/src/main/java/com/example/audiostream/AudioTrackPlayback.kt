package com.example.audiostream

import android.media.AudioTrack
import android.os.Process

internal object AudioTrackPlayback {
    fun run(track: AudioTrack, chunkSamples: Int, shouldContinue: () -> Boolean) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO)
        val output = ShortArray(chunkSamples)
        var playing = false
        var playbackRequest = 0L
        while (shouldContinue()) {
            val request = NativeBridge.nativeGetPlaybackRequest()
            if (request != playbackRequest) {
                if (playing) track.pause()
                track.flush()
                playing = false
                playbackRequest = request
                if (request != 0L && request and 1L == 0L) NativeBridge.nativeReportPlayback(request, false)
            }
            val available = NativeBridge.nativeReadPcm(output)
            if (NativeBridge.nativeGetPlaybackRequest() != request) continue
            if (available == 0) {
                if (playing) {
                    track.pause()
                    track.flush()
                    playing = false
                }
                if (request != 0L) NativeBridge.nativeReportPlayback(request, false)
                Thread.sleep(2)
                continue
            }

            var offset = 0
            while (offset < output.size && shouldContinue()) {
                if (NativeBridge.nativeGetPlaybackRequest() != request) break
                val mode = if (playing) AudioTrack.WRITE_BLOCKING else AudioTrack.WRITE_NON_BLOCKING
                val written = track.write(output, offset, output.size - offset, mode)
                if (written < 0) return
                if (written == 0) {
                    Thread.sleep(1)
                    continue
                }
                offset += written
                if (!playing && shouldContinue() && NativeBridge.nativeGetPlaybackRequest() == request) {
                    track.play()
                    playing = true
                }
            }
            if (request != 0L && playing && offset == output.size && NativeBridge.nativeGetPlaybackRequest() == request) {
                NativeBridge.nativeReportPlayback(request, true)
            }
        }
    }
}
