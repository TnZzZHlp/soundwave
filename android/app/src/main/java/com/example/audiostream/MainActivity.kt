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
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import com.google.mlkit.vision.codescanner.GmsBarcodeScanner
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import java.util.Locale

class MainActivity : Activity() {
    private lateinit var preferences: SharedPreferences
    private lateinit var hostInput: EditText
    private lateinit var portInput: EditText
    private lateinit var fingerprintInput: EditText
    private lateinit var scanPairingQrButton: Button
    private lateinit var statusValue: TextView
    private lateinit var latencyValue: TextView
    private lateinit var bufferValue: TextView
    private lateinit var statisticsValue: TextView
    private lateinit var pairingScanner: GmsBarcodeScanner
    private var scanInProgress = false
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
        scanPairingQrButton = findViewById(R.id.scanPairingQrButton)
        statusValue = findViewById(R.id.statusValue)
        latencyValue = findViewById(R.id.latencyValue)
        bufferValue = findViewById(R.id.bufferValue)
        statisticsValue = findViewById(R.id.statisticsValue)
        pairingScanner = GmsBarcodeScanning.getClient(
            this,
            GmsBarcodeScannerOptions.Builder()
                .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                .enableAutoZoom()
                .build(),
        )

        hostInput.setText(preferences.getString("host", ""))
        portInput.setText(preferences.getInt("port", 48_400).toString())
        fingerprintInput.setText(preferences.getString("fingerprint", ""))

        findViewById<Button>(R.id.connectButton).setOnClickListener { connect() }
        findViewById<Button>(R.id.disconnectButton).setOnClickListener { disconnect() }
        scanPairingQrButton.setOnClickListener { scanPairingQr() }
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
        if (host.isBlank() || port == null || port !in 1..65_535 || !isValidFingerprint(fingerprint)) {
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

    private fun scanPairingQr() {
        if (scanInProgress) {
            return
        }
        scanInProgress = true
        scanPairingQrButton.isEnabled = false
        pairingScanner.startScan()
            .addOnSuccessListener { barcode ->
                val pairing = barcode.rawValue?.let(::parsePairingQr)
                if (pairing == null) {
                    Toast.makeText(this, R.string.pairing_qr_invalid, Toast.LENGTH_LONG).show()
                    return@addOnSuccessListener
                }

                hostInput.setText(pairing.host)
                portInput.setText(pairing.port.toString())
                fingerprintInput.setText(pairing.fingerprint)
                Toast.makeText(this, R.string.pairing_qr_filled, Toast.LENGTH_LONG).show()
            }
            .addOnFailureListener { error ->
                val detail = error.message ?: "unknown error"
                Toast.makeText(
                    this,
                    getString(R.string.pairing_qr_scanner_unavailable, detail),
                    Toast.LENGTH_LONG,
                ).show()
            }
            .addOnCompleteListener {
                scanInProgress = false
                if (!isFinishing && !isDestroyed) {
                    scanPairingQrButton.isEnabled = true
                }
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

    private fun parsePairingQr(payload: String): PairingDetails? {
        if (payload.length > MAX_PAIRING_PAYLOAD_LENGTH || payload.any { it.code !in FIRST_PRINTABLE_ASCII..LAST_PRINTABLE_ASCII }) {
            return null
        }
        if (!payload.startsWith(PAIRING_URI_PREFIX)) {
            return null
        }

        val query = payload.removePrefix(PAIRING_URI_PREFIX)
        if (query.count { it == '&' } != 2) {
            return null
        }
        val parameters = query.split("&")
        if (parameters.size != 3 || !parameters[0].startsWith("host=") || !parameters[1].startsWith("port=") || !parameters[2].startsWith("fp=")) {
            return null
        }
        val host = parameters[0].removePrefix("host=")
        val portText = parameters[1].removePrefix("port=")
        val compactFingerprint = parameters[2].removePrefix("fp=")
        val port = portText.toIntOrNull()
        if (!isValidPairingHost(host)
            || !isCanonicalPort(portText)
            || port == null
            || port !in 1..65_535
            || !isValidCompactFingerprint(compactFingerprint)
        ) {
            return null
        }

        return PairingDetails(
            host = host,
            port = port,
            fingerprint = compactFingerprint
                .uppercase(Locale.ROOT)
                .chunked(2)
                .joinToString(":"),
        )
    }

    private fun isValidPairingHost(host: String): Boolean {
        val octets = host.split(".")
        if (octets.size != 4) {
            return false
        }
        val values = octets.map { octet ->
            if (octet.isEmpty()
                || octet.length > 3
                || (octet.length > 1 && octet.startsWith("0"))
                || octet.any { it !in '0'..'9' }
            ) {
                return false
            }
            octet.toIntOrNull()?.takeIf { it in 0..255 } ?: return false
        }

        val first = values[0]
        return first != 0
            && first != 127
            && first < 224
            && !(first == 169 && values[1] == 254)
    }

    private fun isCanonicalPort(port: String): Boolean {
        return port.isNotEmpty()
            && port.all { it in '0'..'9' }
            && (port.length == 1 || !port.startsWith("0"))
    }

    private fun isValidFingerprint(fingerprint: String): Boolean {
        val compact = fingerprint.filterNot { it == ':' || it == '-' || it.isWhitespace() }
        return isValidCompactFingerprint(compact)
    }

    private fun isValidCompactFingerprint(fingerprint: String): Boolean {
        return fingerprint.length == FINGERPRINT_HEX_LENGTH
            && fingerprint.all { it in '0'..'9' || it in 'A'..'F' || it in 'a'..'f' }
    }

    private data class PairingDetails(
        val host: String,
        val port: Int,
        val fingerprint: String,
    )

    private companion object {
        const val PAIRING_URI_PREFIX = "soundwave://pair/v1?"
        const val MAX_PAIRING_PAYLOAD_LENGTH = 512
        const val FIRST_PRINTABLE_ASCII = 0x21
        const val LAST_PRINTABLE_ASCII = 0x7E
        const val FINGERPRINT_HEX_LENGTH = 64
    }
}
