package com.example.audiostream

import android.media.AudioDeviceInfo

fun main() {
    val phoneOutputs = intArrayOf(
        AudioDeviceInfo.TYPE_BUILTIN_EARPIECE,
        AudioDeviceInfo.TYPE_BUILTIN_SPEAKER,
        AudioDeviceInfo.TYPE_BUILTIN_SPEAKER_SAFE,
        AudioDeviceInfo.TYPE_TELEPHONY,
    )
    check(CalibrationOutputPolicy.isSpeakerOnly(phoneOutputs))
    for (external in intArrayOf(
        AudioDeviceInfo.TYPE_WIRED_HEADSET,
        AudioDeviceInfo.TYPE_BLUETOOTH_A2DP,
        AudioDeviceInfo.TYPE_USB_HEADSET,
        AudioDeviceInfo.TYPE_HDMI,
        AudioDeviceInfo.TYPE_REMOTE_SUBMIX,
        AudioDeviceInfo.TYPE_UNKNOWN,
    )) {
        check(!CalibrationOutputPolicy.isSpeakerOnly(phoneOutputs + external))
    }
    check(!CalibrationOutputPolicy.isSpeakerOnly(intArrayOf()))
}
