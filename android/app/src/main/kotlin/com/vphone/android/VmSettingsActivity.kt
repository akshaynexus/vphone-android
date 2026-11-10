package com.vphone.android

import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.DialogHost
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.SettingsScreen
import com.vphone.android.ui.VmSettings
import com.vphone.android.ui.VphoneTheme
import org.json.JSONArray
import org.json.JSONObject

/** One VM's settings (`<vm>/android.json`, `VmConfig` on the native side), as a screen. */
class VmSettingsActivity : ComponentActivity() {
    private val dialogs = DialogHost()

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val name = intent.getStringExtra(EXTRA_VM).orEmpty()
        val vms = JSONArray(Native.listVms())
        val config = (0 until vms.length()).map { vms.getJSONObject(it) }
            .firstOrNull { it.getString("name") == name }?.getJSONObject("config")
        if (config == null) {
            Toast.makeText(this, "$name is no longer on this phone", Toast.LENGTH_LONG).show()
            finish()
            return
        }
        val info = JSONObject(Native.deviceInfo())
        val initial = VmSettings(
            version = config.optInt("version", 2),
            vcpus = config.getInt("vcpus"),
            ramMib = config.getInt("ramMib"),
            tbMib = config.getInt("tbMib"),
            performance = config.optString("performance", "max"),
            refreshHz = config.optInt("refreshHz", 120),
            performanceHints = config.getBoolean("performanceHints"),
            services = config.optString("services", "unchanged"),
            network = config.optBoolean("network", true),
            sound = config.optBoolean("sound", true),
            display = config.optBoolean("display", true),
            gpuCompression = config.optBoolean("gpuCompression", false),
            gpuDiagnostics = config.optBoolean("gpuDiagnostics", false),
            softwareDisplay = config.optBoolean("softwareDisplay", false),
            swapchainLayout = config.optString("swapchainLayout", "auto"),
            lighterVisuals = config.optBoolean("lighterVisuals", true),
            longerLaunchLimits = config.optBoolean("longerLaunchLimits", true),
            verboseKernelLog = config.optBoolean("verboseKernelLog", false),
            rootBoost = config.optBoolean("rootBoost", false),
        )
        setContent {
            VphoneTheme {
                SettingsScreen(
                    name = name,
                    initial = initial,
                    cpus = info.getInt("cpus"),
                    totalMib = info.getLong("totalMib"),
                    onSave = { save(name, config, it) },
                    onBack = ::finish,
                )
                dialogs.current?.let { spec ->
                    AppDialog(spec, onCopy = { App.copy(this, it) }, onDismiss = { dialogs.current = null })
                }
            }
        }
    }

    /**
     * The native side checks the values; what it refuses is shown, and the screen stays. The
     * save starts from the settings as they were read, so a field this screen does not know yet
     * keeps its value instead of going back to its default.
     */
    private fun save(name: String, read: JSONObject, s: VmSettings) {
        val saved = JSONObject(read.toString())
            .put("vcpus", s.vcpus)
            .put("ramMib", s.ramMib)
            .put("tbMib", s.tbMib)
            .put("version", s.version)
            .put("performance", s.performance)
            .put("refreshHz", s.refreshHz)
            .put("performanceHints", s.performanceHints)
            .put("services", s.services)
            .put("network", s.network)
            .put("sound", s.sound)
            .put("display", s.display)
            .put("gpuCompression", s.gpuCompression)
            .put("gpuDiagnostics", s.gpuDiagnostics)
            .put("softwareDisplay", s.softwareDisplay)
            .put("swapchainLayout", s.swapchainLayout)
            .put("lighterVisuals", s.lighterVisuals)
            .put("longerLaunchLimits", s.longerLaunchLimits)
            .put("verboseKernelLog", s.verboseKernelLog)
            .put("rootBoost", s.rootBoost)
        val error = Native.saveVmConfig(name, saved.toString())
        if (error.isNotEmpty()) {
            dialogs.show(DialogSpec.error("$name's settings were not saved", error))
        } else {
            Toast.makeText(this, "Saved", Toast.LENGTH_SHORT).show()
            finish()
        }
    }

    companion object {
        const val EXTRA_VM = "vm"
    }
}
