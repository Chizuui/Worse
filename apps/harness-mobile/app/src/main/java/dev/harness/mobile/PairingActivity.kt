package dev.harness.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.text.InputType
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions

/**
 * First-run screen. Accepts `https://<host>/p#t=<token>` from the QR code or the clipboard, stores
 * the origin encrypted, then hands the full link to [MainActivity] once so the page can exchange the
 * token for a device cookie.
 */
class PairingActivity : AppCompatActivity() {

    private lateinit var status: TextView

    private val scanLauncher = registerForActivityResult(ScanContract()) { result ->
        val contents = result.contents
        if (contents.isNullOrBlank()) {
            setStatus(R.string.pairing_bad_link)
            return@registerForActivityResult
        }
        accept(contents)
    }

    private val cameraPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            launchScanner()
        } else {
            setStatus(R.string.pairing_camera_needed)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Already paired? Go straight to the app.
        if (PairStore.origin(this) != null) {
            openMain(pairUrl = null)
            return
        }

        setContentView(R.layout.activity_pairing)
        status = findViewById(R.id.status)

        val input = findViewById<EditText>(R.id.linkInput)
        input.setRawInputType(InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI)

        findViewById<Button>(R.id.scanButton).setOnClickListener { startScan() }
        findViewById<Button>(R.id.connectButton).setOnClickListener {
            accept(input.text?.toString())
        }

        intent?.getStringExtra(MainActivity.EXTRA_PAIR_URL)?.let { accept(it) }
    }

    private fun startScan() {
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            launchScanner()
        } else {
            cameraPermission.launch(Manifest.permission.CAMERA)
        }
    }

    private fun launchScanner() {
        val options = ScanOptions()
            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
            .setPrompt(getString(R.string.pairing_scan))
            .setBeepEnabled(false)
            .setOrientationLocked(false)
        scanLauncher.launch(options)
    }

    private fun accept(raw: String?) {
        val link = PairStore.parse(raw)
        if (link == null) {
            setStatus(R.string.pairing_bad_link)
            return
        }
        PairStore.saveOrigin(this, link.origin)
        openMain(link.url)
    }

    private fun openMain(pairUrl: String?) {
        val intent = Intent(this, MainActivity::class.java)
        if (pairUrl != null) intent.putExtra(MainActivity.EXTRA_PAIR_URL, pairUrl)
        startActivity(intent)
        finish()
    }

    private fun setStatus(resId: Int) {
        if (!::status.isInitialized) return
        status.setText(resId)
        status.visibility = View.VISIBLE
    }
}
