package com.example.audiostream

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Log
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * In-app camera QR scanner. Captures pairing data with CameraX and decodes it
 * with ZXing, so scanning does not depend on Google Play services.
 */
class QrScannerActivity : ComponentActivity() {
    private lateinit var previewView: PreviewView
    private lateinit var hintView: TextView
    private lateinit var analysisExecutor: ExecutorService
    private val analyzer = QrCodeAnalyzer(::onDecoded)
    private var cameraProvider: ProcessCameraProvider? = null
    private var handled = false
    private val cameraPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) {
            startCamera()
        } else {
            hintView.setText(R.string.pairing_qr_camera_denied)
            finishAfterHint()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_qr_scanner)
        previewView = findViewById(R.id.cameraPreview)
        hintView = findViewById(R.id.scannerHint)
        analysisExecutor = Executors.newSingleThreadExecutor()

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            startCamera()
        } else {
            cameraPermission.launch(Manifest.permission.CAMERA)
        }
    }

    override fun onDestroy() {
        cameraProvider?.unbindAll()
        analysisExecutor.shutdown()
        super.onDestroy()
    }

    private fun startCamera() {
        val providerFuture = ProcessCameraProvider.getInstance(this)
        providerFuture.addListener(
            {
                try {
                    bindCamera(providerFuture.get())
                } catch (error: Exception) {
                    Log.w(TAG, "Camera unavailable", error)
                    hintView.setText(R.string.pairing_qr_camera_unavailable)
                    finishAfterHint()
                }
            },
            ContextCompat.getMainExecutor(this),
        )
    }

    private fun bindCamera(provider: ProcessCameraProvider) {
        cameraProvider = provider
        val preview = Preview.Builder().build().also {
            it.surfaceProvider = previewView.surfaceProvider
        }
        val analysis = ImageAnalysis.Builder()
            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
            .build()
            .also { it.setAnalyzer(analysisExecutor, analyzer) }

        val selector = when {
            provider.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA) -> CameraSelector.DEFAULT_BACK_CAMERA
            provider.hasCamera(CameraSelector.DEFAULT_FRONT_CAMERA) -> CameraSelector.DEFAULT_FRONT_CAMERA
            else -> throw IllegalStateException("No camera available")
        }

        provider.unbindAll()
        provider.bindToLifecycle(this, selector, preview, analysis)
    }

    private fun onDecoded(payload: String) {
        runOnUiThread {
            if (handled) {
                return@runOnUiThread
            }
            val pairing = PairingPayload.parse(payload)
            if (pairing == null) {
                hintView.setText(R.string.pairing_qr_not_pairing_code)
                return@runOnUiThread
            }

            handled = true
            cameraProvider?.unbindAll()
            setResult(
                RESULT_OK,
                Intent().apply {
                    putExtra(EXTRA_HOST, pairing.host)
                    putExtra(EXTRA_PORT, pairing.port)
                    putExtra(EXTRA_FINGERPRINT, pairing.fingerprint)
                },
            )
            finish()
        }
    }

    private fun finishAfterHint() {
        handled = true
        hintView.postDelayed({ if (!isFinishing) finish() }, HINT_DURATION_MS)
    }

    companion object {
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_FINGERPRINT = "fingerprint"
        private const val HINT_DURATION_MS = 2_500L
        private const val TAG = "QrScannerActivity"
    }
}
