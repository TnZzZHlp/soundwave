package com.example.audiostream

import android.media.AudioDeviceInfo

internal object CalibrationOutputPolicy {
    fun isSpeakerOnly(outputTypes: IntArray): Boolean {
        val hasSpeaker = outputTypes.any {
            it == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER || it == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER_SAFE
        }
        return hasSpeaker && outputTypes.all {
            it == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER ||
                it == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER_SAFE ||
                it == AudioDeviceInfo.TYPE_BUILTIN_EARPIECE ||
                it == AudioDeviceInfo.TYPE_TELEPHONY
        }
    }
}
