package com.vphone.android

import android.annotation.SuppressLint
import android.content.Intent
import android.net.Uri
import android.os.BatteryManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.view.View
import android.widget.ArrayAdapter
import android.widget.AutoCompleteTextView
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.activity.addCallback
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButtonToggleGroup
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.materialswitch.MaterialSwitch
import com.google.android.material.progressindicator.LinearProgressIndicator
import com.google.android.material.textfield.TextInputEditText
import org.json.JSONObject
import java.util.Locale

/** The wizard: pick a device and iOS version, then follow the creation to the running VM. */
class CreateActivity : AppCompatActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private var following: String? = null
    private lateinit var catalog: JSONObject
    private lateinit var pairingsByDevice: Map<String, List<JSONObject>>
    private var pickedDevice = ""
    private var pickedIos: JSONObject? = null
    private var rowsFor = ""
    private val rows = HashMap<String, View>()

    private val poll = object : Runnable {
        override fun run() {
            following?.let { showStatus(JSONObject(Native.createStatus(it))) }
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pending = Native.createPending()
        if (pending.isNotEmpty()) follow(pending) else showPicker()
        onBackPressedDispatcher.addCallback(this) { finish() }
    }

    override fun onStart() {
        super.onStart()
        handler.post(poll)
    }

    override fun onStop() {
        handler.removeCallbacks(poll)
        super.onStop()
    }

    // ------------------------------------------------------------------ the pickers

    private fun showPicker() {
        following = null
        setContentView(R.layout.create_pick)
        catalog = JSONObject(Native.createCatalog())
        val devices = catalog.getJSONArray("devices")
        val all = (0 until devices.length()).map { devices.getJSONObject(it) }
        pairingsByDevice = all.associate { d ->
            val p = d.getJSONArray("pairings")
            d.getString("id") to (0 until p.length()).map { p.getJSONObject(it) }
        }
        findViewById<TextView>(R.id.about).text = ABOUT

        val deviceView = findViewById<AutoCompleteTextView>(R.id.device)
        val deviceLabels = all.map { "${it.getString("name")} (${it.getString("id")})" }
        deviceView.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, deviceLabels))
        deviceView.setOnItemClickListener { _, _, position, _ ->
            selectDevice(all[position].getString("id"))
        }
        val iosView = findViewById<AutoCompleteTextView>(R.id.ios)
        iosView.setOnItemClickListener { _, _, position, _ ->
            selectIos(pairingsByDevice.getValue(pickedDevice)[position])
        }
        findViewById<Button>(R.id.start).setOnClickListener { confirmAndStart() }
        findViewById<Button>(R.id.import_instead).setOnClickListener {
            startActivity(Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_IMPORT, true))
            finish()
        }

        val def = catalog.getString("defaultDevice")
        val i = all.indexOfFirst { it.getString("id") == def }.coerceAtLeast(0)
        deviceView.setText(deviceLabels[i], false)
        selectDevice(all[i].getString("id"))
        val defaultIos = pairingsByDevice.getValue(pickedDevice)
            .firstOrNull { it.getString("ios") == catalog.getString("defaultIos") }
        defaultIos?.let {
            iosView.setText(labelFor(it), false)
            selectIos(it)
        }
    }

    private fun labelFor(p: JSONObject): String {
        val tag = when (p.getString("status")) {
            "tested" -> "tested"
            else -> "under test"
        }
        val rec = if (p.optBoolean("recommended")) ", recommended" else ""
        return "iOS ${p.getString("ios")} (build ${p.getString("build")}), $tag$rec"
    }

    private fun selectDevice(id: String) {
        pickedDevice = id
        val pairings = pairingsByDevice.getValue(id)
        val iosView = findViewById<AutoCompleteTextView>(R.id.ios)
        iosView.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, pairings.map { labelFor(it) }))
        val first = pairings.firstOrNull { it.optBoolean("recommended") }
            ?: pairings.firstOrNull { it.getString("status") == "tested" }
            ?: pairings.first()
        iosView.setText(labelFor(first), false)
        selectIos(first)
    }

    @SuppressLint("SetTextI18n")
    private fun selectIos(p: JSONObject) {
        pickedIos = p
        findViewById<TextView>(R.id.pairing_note).text = p.optString("note")
        val warning = findViewById<TextView>(R.id.warning)
        val underTest = p.getString("status") != "tested"
        warning.visibility = if (underTest) View.VISIBLE else View.GONE
        if (underTest) {
            warning.text = "Under test: this pairing has been verified on macOS only, not with the Linux flow this app follows. " +
                "Expect it may need manual attention, and hours of work to be lost if it fails."
        }
        val minScale = p.optInt("minScale", 1)
        val group = findViewById<MaterialButtonToggleGroup>(R.id.scale)
        findViewById<Button>(R.id.scale1).isEnabled = minScale <= 1
        group.check(if (minScale <= 1) R.id.scale1 else R.id.scale2)
        findViewById<TextInputEditText>(R.id.name).setText(
            "vphone-" + pickedDevice.replace(",", "-") + "-" + p.getString("ios").replace(".", ""),
        )
        updatePreflight()
    }

    private fun specJson(): JSONObject {
        val p = pickedIos!!
        val group = findViewById<MaterialButtonToggleGroup>(R.id.scale)
        val scale = when (group.checkedButtonId) {
            R.id.scale3 -> 3
            R.id.scale2 -> 2
            else -> 1
        }
        return JSONObject()
            .put("name", findViewById<TextInputEditText>(R.id.name).text.toString())
            .put("device", pickedDevice)
            .put("ios", p.getString("ios"))
            .put("build", p.getString("build"))
            .put("scale", scale)
            .put("allowCellular", findViewById<MaterialSwitch>(R.id.cellular).isChecked)
            .put("deleteFirmwareAfter", findViewById<MaterialSwitch>(R.id.delete_fw).isChecked)
    }

    @SuppressLint("SetTextI18n")
    private fun updatePreflight(): JSONObject {
        val report = JSONObject(Native.createPreflight(specJson().toString()))
        val line = StringBuilder()
        line.append("Time: about ${report.optLong("lowHours")} to ${report.optLong("highHours")} hours on this phone.\n")
        line.append("Storage free: ${report.optLong("freeGb")} GB (about ${report.optLong("wantGb")} GB will be used).")
        val errors = report.getJSONArray("errors")
        val warnings = report.getJSONArray("warnings")
        for (i in 0 until errors.length()) line.append("\n✕ ").append(errors.getString(i))
        for (i in 0 until warnings.length()) line.append("\n⚠ ").append(warnings.getString(i))
        findViewById<TextView>(R.id.preflight).text = line
        findViewById<Button>(R.id.start).isEnabled = errors.length() == 0
        return report
    }

    private fun charging(): Boolean {
        val bm = getSystemService(BatteryManager::class.java)
        return bm.isCharging
    }

    private fun confirmAndStart() {
        val spec = specJson()
        if (updatePreflight().getJSONArray("errors").length() > 0) return
        val battery = if (charging()) "" else "\n\nThe phone is not charging. Plug it in: this draws power for hours and may heat the phone."
        MaterialAlertDialogBuilder(this)
            .setTitle("Create ${spec.getString("name")}?")
            .setMessage("$TIME_WARNING$battery")
            .setPositiveButton("Start") { _, _ -> maybeAskBattery(spec) }
            .setNegativeButton("Back", null)
            .show()
    }

    /** Without this, Samsung's and OnePlus's battery managers stop the app in the background. */
    private fun maybeAskBattery(spec: JSONObject) {
        val power = getSystemService(PowerManager::class.java)
        if (power.isIgnoringBatteryOptimizations(packageName)) {
            start(spec)
            return
        }
        MaterialAlertDialogBuilder(this)
            .setTitle("Keep it running with the screen off")
            .setMessage("Without unrestricted battery use, this phone's battery manager can stop creation when the app is not on screen. Allow it for vphone.")
            .setPositiveButton("Allow") { _, _ ->
                startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
                start(spec)
            }
            .setNegativeButton("Not now") { _, _ -> start(spec) }
            .show()
    }

    private fun start(spec: JSONObject) {
        val error = Native.createBegin(spec.toString())
        if (error.isNotEmpty()) {
            MaterialAlertDialogBuilder(this).setTitle("Cannot start").setMessage(error).setPositiveButton("OK", null).show()
            return
        }
        val name = spec.getString("name")
        CreateService.start(this, name)
        follow(name)
    }

    // ------------------------------------------------------------------ following one

    private fun follow(name: String) {
        following = name
        rowsFor = ""
        setContentView(R.layout.create_progress)
        findViewById<Button>(R.id.cancel).setOnClickListener { confirmCancel(name) }
        findViewById<Button>(R.id.delete).setOnClickListener { confirmDelete(name) }
        findViewById<Button>(R.id.retry).setOnClickListener { CreateService.start(this, name) }
        findViewById<Button>(R.id.use_cellular).setOnClickListener {
            MaterialAlertDialogBuilder(this)
                .setTitle("Download over cellular?")
                .setMessage("The remaining firmware (up to about 12 GB) would use your mobile data.")
                .setPositiveButton("Use cellular") { _, _ -> Native.createAllowCellular(name, true) }
                .setNegativeButton("Wait for Wi-Fi", null)
                .show()
        }
        findViewById<Button>(R.id.launch).setOnClickListener {
            startActivity(Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_BOOT, name))
            finish()
        }
    }

    private fun confirmCancel(name: String) {
        MaterialAlertDialogBuilder(this)
            .setTitle("Cancel creating this VM?")
            .setMessage("It stops the current step. You can resume it later from where it stopped, or delete it.")
            .setPositiveButton("Stop it") { _, _ -> Native.createCancel(name) }
            .setNegativeButton("Keep going", null)
            .show()
    }

    private fun confirmDelete(name: String) {
        val checks = arrayOf("Also delete the cached firmware")
        val checked = booleanArrayOf(false)
        MaterialAlertDialogBuilder(this)
            .setTitle("Delete $name?")
            .setMultiChoiceItems(checks, checked) { _, which, value -> checked[which] = value }
            .setPositiveButton("Delete") { _, _ ->
                Native.createCancel(name)
                val error = Native.createDelete(name, checked[0])
                if (error.isNotEmpty()) Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                finish()
            }
            .setNegativeButton("Keep it", null)
            .show()
    }

    @SuppressLint("SetTextI18n")
    private fun showStatus(s: JSONObject) {
        val state = s.optString("state")
        findViewById<TextView>(R.id.title).text = "Creating ${s.optString("name")} (${s.optString("device")}, iOS ${s.optString("ios")})"
        val eta = s.optLong("etaSecs")
        findViewById<TextView>(R.id.eta).text = when (state) {
            "done" -> "Finished."
            "failed" -> "Stopped: ${s.optString("error").ifEmpty { "see the log" }}"
            "cancelled" -> "Cancelled. Resume it or delete it."
            "interrupted" -> "Interrupted. Tap Resume to continue."
            else -> if (eta > 0) "About ${hoursMinutes(eta)} left (estimate; a phone's speed varies)" else "Working"
        }

        val hold = findViewById<TextView>(R.id.hold)
        val holdKey = s.optString("hold")
        hold.visibility = if (holdKey.isNotEmpty()) View.VISIBLE else View.GONE
        hold.text = when (holdKey) {
            "wifi" -> "On hold: waiting for Wi-Fi."
            "network" -> "On hold: waiting for a connection."
            "storage" -> "On hold: storage is low. ${s.optString("message")}"
            "thermal" -> "On hold: the phone is cooling down."
            else -> ""
        }
        findViewById<Button>(R.id.use_cellular).visibility = if (holdKey == "wifi") View.VISIBLE else View.GONE

        val thermal = findViewById<TextView>(R.id.thermal)
        val hot = s.optInt("thermal")
        thermal.visibility = if (hot >= 2) View.VISIBLE else View.GONE
        thermal.text = when (hot) {
            2 -> "The phone is warm; this may be slower."
            3 -> "The phone is hot; this will be slower."
            else -> "The phone is overheating: let it cool down."
        }

        findViewById<TextView>(R.id.message).text = s.optString("message").let {
            val speed = s.optLong("bytesPerSec")
            if (speed > 0) "$it (${speed / MIB} MB/s)" else it
        }
        val bar = findViewById<LinearProgressIndicator>(R.id.bar)
        val total = s.optLong("totalBytes")
        bar.isIndeterminate = state == "running" && total == 0L
        if (total > 0) bar.setProgressCompat((1000 * s.optLong("doneBytes") / total).toInt(), true)
        bar.visibility = if (state == "done" || state == "failed" || state == "cancelled") View.GONE else View.VISIBLE

        renderSteps(s)
        findViewById<Button>(R.id.launch).visibility = if (state == "done") View.VISIBLE else View.GONE
        val stopped = state == "failed" || state == "cancelled" || state == "interrupted"
        findViewById<Button>(R.id.retry).visibility = if (stopped) View.VISIBLE else View.GONE
        findViewById<Button>(R.id.retry).text = if (state == "failed") "Retry from the failed step" else "Resume"
        findViewById<Button>(R.id.cancel).visibility = if (state == "running") View.VISIBLE else View.GONE
    }

    private fun renderSteps(s: JSONObject) {
        val container = findViewById<LinearLayout>(R.id.steps)
        val steps = s.getJSONArray("steps")
        if (rowsFor != s.optString("name")) {
            rowsFor = s.optString("name")
            rows.clear()
            container.removeAllViews()
            for (i in 0 until steps.length()) {
                val st = steps.getJSONObject(i)
                val row = layoutInflater.inflate(R.layout.create_step_row, container, false)
                row.findViewById<TextView>(R.id.step_title).text = st.getString("title")
                row.findViewById<Button>(R.id.step_log).setOnClickListener {
                    App.showLog(this, st.getString("log"))
                }
                rows[st.getString("key")] = row
                container.addView(row)
            }
        }
        for (i in 0 until steps.length()) {
            val st = steps.getJSONObject(i)
            val row = rows[st.getString("key")] ?: continue
            val state = st.getString("state")
            row.findViewById<TextView>(R.id.mark).text = when (state) {
                "done" -> "✓"
                "running" -> "▶"
                "failed" -> "✕"
                else -> "·"
            }
            val est = st.optLong("estSecs")
            val elapsed = st.optLong("elapsedSecs")
            row.findViewById<TextView>(R.id.step_time).text = when {
                state == "done" -> if (elapsed > 0) "took ${hoursMinutes(elapsed)}" else "done"
                state == "running" -> "about ${hoursMinutes(est)} estimated"
                else -> "about ${hoursMinutes(est)}"
            }
        }
    }

    private fun hoursMinutes(secs: Long): String {
        val h = secs / 3600
        val m = (secs % 3600) / 60
        return if (h > 0) String.format(Locale.US, "%d h %02d min", h, m) else "$m min"
    }

    companion object {
        private const val MIB = 1024L * 1024

        const val TIME_WARNING =
            "This makes the VM entirely on this phone: it downloads about 12 GB from Apple, patches the " +
                "firmware, and runs iOS's own installer in emulation, several boots of it. Expect it to take many " +
                "hours, probably overnight. It carries on with the screen off and survives the phone restarting. " +
                "Keep the phone plugged in and in a cool place."

        private const val ABOUT =
            "Builds a VM from Apple's firmware with no computer and no root. The pairing marked tested has been " +
                "verified end to end; others may need extra work.\n\n" +
                "You need about 60 GB free while it runs (the finished VM takes 20 to 25 GB) and 8 GB of RAM or more. " +
                "Firmware is reused for later VMs unless you choose to delete it."
    }
}
