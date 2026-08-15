package com.example.audiostream

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast

class MainActivity : Activity() {
    private lateinit var preferences: SharedPreferences
    private lateinit var hostInput: EditText
    private lateinit var portInput: EditText
    private lateinit var fingerprintInput: EditText
    private lateinit var statusValue: TextView
    private lateinit var latencyValue: TextView
    private lateinit var bufferValue: TextView
    private lateinit var statisticsValue: TextView
    private val handler = Handler(Looper.getMainLooper())

    private val refreshStatus = object : Runnable {
        override fun run() {
            renderNativeState()
            handler.postDelayed(this, 1_000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        NativeBridge.nativeInit()
        preferences = getSharedPreferences("connection", MODE_PRIVATE)
        hostInput = findViewById(R.id.serverHost)
        portInput = findViewById(R.id.serverPort)
        fingerprintInput = findViewById(R.id.certificateFingerprint)
        statusValue = findViewById(R.id.statusValue)
        latencyValue = findViewById(R.id.latencyValue)
        bufferValue = findViewById(R.id.bufferValue)
        statisticsValue = findViewById(R.id.statisticsValue)

        hostInput.setText(preferences.getString("host", ""))
        portInput.setText(preferences.getInt("port", 48_400).toString())
        fingerprintInput.setText(preferences.getString("fingerprint", ""))

        findViewById<Button>(R.id.connectButton).setOnClickListener { connect() }
        findViewById<Button>(R.id.disconnectButton).setOnClickListener { disconnect() }
        requestNotificationPermissionIfNeeded()
    }

    override fun onResume() {
        super.onResume()
        handler.post(refreshStatus)
    }

    override fun onPause() {
        handler.removeCallbacks(refreshStatus)
        super.onPause()
    }

    private fun connect() {
        val host = hostInput.text.toString().trim()
        val port = portInput.text.toString().toIntOrNull()
        val fingerprint = fingerprintInput.text.toString().trim()
        if (host.isBlank() || port == null || port !in 1..65_535 || fingerprint.isBlank()) {
            Toast.makeText(this, "Enter server IP, port, and TLS fingerprint", Toast.LENGTH_LONG).show()
            return
        }
        preferences.edit()
            .putString("host", host)
            .putInt("port", port)
            .putString("fingerprint", fingerprint)
            .apply()

        val intent = Intent(this, AudioService::class.java).apply {
            action = AudioService.ACTION_CONNECT
            putExtra(AudioService.EXTRA_HOST, host)
            putExtra(AudioService.EXTRA_PORT, port)
            putExtra(AudioService.EXTRA_FINGERPRINT, fingerprint)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
    }

    private fun disconnect() {
        startService(Intent(this, AudioService::class.java).setAction(AudioService.ACTION_DISCONNECT))
    }

    private fun renderNativeState() {
        val state = NativeBridge.nativeGetState()
        statusValue.text = when (state) {
            NativeBridge.STATE_CONNECTING -> "Connecting"
            NativeBridge.STATE_CONNECTED -> "Connected"
            NativeBridge.STATE_DISCONNECTED -> "Disconnected"
            NativeBridge.STATE_ERROR -> "Error: ${NativeBridge.nativeGetLastError()}"
            else -> "Idle"
        }
        latencyValue.text = "Estimated latency: ${NativeBridge.nativeGetLatencyMs()} ms"
        bufferValue.text = "Buffer: ${NativeBridge.nativeGetBufferMs()} ms"
        statisticsValue.text = "Packets: ${NativeBridge.nativeGetReceivedPackets()}  lost: ${NativeBridge.nativeGetLostPackets()}  late: ${NativeBridge.nativeGetLatePackets()}  underruns: ${NativeBridge.nativeGetUnderruns()}"
    }

    private fun requestNotificationPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }
}
