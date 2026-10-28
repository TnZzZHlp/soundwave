package com.example.audiostream

import android.media.AudioTrack
import android.os.Process

internal object AudioTrackPlayback {
    fun run(track: AudioTrack, chunkSamples: Int, shouldContinue: () -> Boolean) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO)
        val output = ShortArray(chunkSamples)
        var playing = false
        while (shouldContinue()) {
            val available = NativeBridge.nativeReadPcm(output)
            if (available == 0) {
                if (playing) {
                    track.pause()
                    track.flush()
                    playing = false
                }
                Thread.sleep(2)
                continue
            }

            var offset = 0
            while (offset < output.size && shouldContinue()) {
                val mode = if (playing) AudioTrack.WRITE_BLOCKING else AudioTrack.WRITE_NON_BLOCKING
                val written = track.write(output, offset, output.size - offset, mode)
                if (written < 0) return
                if (written == 0) {
                    Thread.sleep(1)
                    continue
                }
                offset += written
                if (!playing && shouldContinue()) {
                    track.play()
                    playing = true
                }
            }
        }
    }
}
