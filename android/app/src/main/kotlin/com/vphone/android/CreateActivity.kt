package com.vphone.android

import android.annotation.SuppressLint
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Typeface
import android.net.Uri
import android.os.BatteryManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.view.View
import android.widget.ArrayAdapter
import android.widget.AutoCompleteTextView
import android.widget.Button
import android.widget.GridLayout
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.activity.addCallback
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButtonToggleGroup
import com.google.android.material.color.MaterialColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.materialswitch.MaterialSwitch
import com.google.android.material.progressindicator.LinearProgressIndicator
import com.google.android.material.textfield.TextInputEditText
import org.json.JSONArray
import org.json.JSONObject
import java.util.Locale
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/** The wizard: pick a device and iOS version, then follow the creation to the running VM. */
class CreateActivity : AppCompatActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private var following: String? = null
    private var lastStatus: JSONObject? = null
    private var paused = false
    private lateinit var catalog: JSONObject
    private lateinit var pairingsByDevice: Map<String, List<JSONObject>>
    private var pickedDevice = ""
    private var pickedIos: JSONObject? = null
    private var rowsFor = ""
    private val rows = HashMap<String, View>()
    private var stageText = ""
    private val worker = Executors.newSingleThreadExecutor()
    private val polling = AtomicBoolean(false)

    private val poll = object : Runnable {
        override fun run() {
            following?.let(::refresh)
            handler.postDelayed(this, 1000)
        }
    }

    /** Reading the status and the step's log is file work, so it happens off the main thread. */
    private fun refresh(name: String) {
        if (!polling.compareAndSet(false, true)) return
        worker.execute {
            val status = runCatching { JSONObject(Native.createStatus(name)) }.getOrNull()
            val live = runCatching { JSONObject(Native.createLive(name)) }.getOrNull()
            polling.set(false)
            handler.post {
                if (following == name) {
                    status?.let(::showStatus)
                    live?.let(::showLive)
                }
            }
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

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    // ------------------------------------------------------------------ the pickers

    private fun showPicker() {
        following = null
        setContentView(R.layout.create_pick)
        App.fitSystemBars(findViewById(R.id.pick_scroll))
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
        line.append(report.optString("estimate")).append("\n")
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
        App.fitSystemBars(findViewById(R.id.progress_root))
        findViewById<Button>(R.id.copy_error).setOnClickListener { copyReport() }
        findViewById<Button>(R.id.live_log).setOnClickListener { CreateLogActivity.open(this, name) }
        findViewById<Button>(R.id.watch_vm).setOnClickListener {
            startActivity(Intent(this, CreateVmScreen::class.java))
        }
        findViewById<Button>(R.id.pause).setOnClickListener {
            if (paused) Native.createPause(name, false) else confirmPause(name)
        }
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

    /** What a pause keeps and what it cannot, said before it starts rather than found out after. */
    private fun confirmPause(name: String) {
        val s = lastStatus
        val key = s?.optString("step").orEmpty()
        val steps = s?.optJSONArray("steps")
        val title = (0 until (steps?.length() ?: 0)).map { steps!!.getJSONObject(it) }
            .firstOrNull { it.optString("key") == key }?.optString("title").orEmpty()
        val step = if (title.isEmpty()) "the current step" else "the current step (\u201c$title\u201d)"
        val message = if (key == "firmware") {
            "The download stops after the piece it is fetching and carries on from there when you resume, " +
                "also after the app or the phone restarts. Nothing downloaded is lost."
        } else {
            "Kept: the VM and its tools stop exactly where they are and stay in memory. Resume carries on from " +
                "the same point, however long the pause was: waiting does not use up any time limit.\n\n" +
                "Not kept: if the app is closed, the phone restarts, or Android ends the app to free memory while " +
                "it is paused, $step starts again from its beginning. Steps already finished are never redone.\n\n" +
                "While paused, the VM still holds its memory, so other apps have less of it."
        }
        MaterialAlertDialogBuilder(this)
            .setTitle("Pause now?")
            .setMessage(message)
            .setPositiveButton("Pause") { _, _ -> Native.createPause(name, true) }
            .setNegativeButton("Keep running", null)
            .show()
    }

    private fun confirmCancel(name: String) {
        MaterialAlertDialogBuilder(this)
            .setTitle("Stop creating this VM?")
            .setMessage("It ends the current step. You can resume later from the start of that step. To carry on without losing the step, use Pause instead.")
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

    /** The error, the phone and the failed step's log, ready to paste into a bug report. */
    private fun copyReport() {
        val s = lastStatus ?: return
        val steps = s.getJSONArray("steps")
        val failed = (0 until steps.length()).map { steps.getJSONObject(it) }
            .firstOrNull { it.getString("state") == "failed" }
        val report = buildString {
            appendLine("vphone ${BuildConfig.VERSION_NAME}, ${s.optString("device")} iOS ${s.optString("ios")}")
            appendLine("${Build.MANUFACTURER} ${Build.MODEL}, ${Build.SOC_MODEL}, Android ${Build.VERSION.RELEASE}")
            failed?.let { appendLine("Failed step: ${it.getString("key")}") }
            appendLine()
            appendLine(s.optString("error"))
            failed?.let {
                appendLine()
                appendLine("Log:")
                append(Native.logTail(it.getString("log"), 16384))
            }
        }
        App.copy(this, report)
    }

    @SuppressLint("SetTextI18n")
    private fun showStatus(s: JSONObject) {
        lastStatus = s
        val state = s.optString("state")
        paused = s.optBoolean("paused")
        val failed = state == "failed"
        findViewById<TextView>(R.id.title).text = "${s.optString("name")} \u00b7 ${s.optString("device")}, iOS ${s.optString("ios")}"
        val all = s.optJSONArray("steps")
        val stepList = (0 until (all?.length() ?: 0)).map { all!!.getJSONObject(it) }
        val at = stepList.indexOfFirst { it.optString("key") == s.optString("step") }
        findViewById<TextView>(R.id.now_step).text = when {
            state == "done" -> "All ${stepList.size} steps finished"
            at >= 0 -> "Step ${at + 1} of ${stepList.size} \u00b7 ${stepList[at].optString("title")}"
            else -> "Getting ready"
        }
        // The time left is only ever made of times this phone measured; without them, the reason.
        val left = s.optLong("etaSecs")
        val estimate = when {
            left > 0 -> "About ${durationText(left)} left, going by this phone's last run of these steps"
            else -> s.optString("etaNote")
        }
        findViewById<TextView>(R.id.eta).text = when (state) {
            "done" -> "Finished."
            "failed" -> "Stopped with an error."
            "cancelled" -> "Stopped. Resume it or delete it."
            "interrupted" -> if (paused) "Paused. Tap Resume to carry on." else "Interrupted. Tap Resume to continue."
            else -> {
                val running = if (paused) "Paused: tap Resume to carry on" else "Running"
                if (estimate.isEmpty()) "$running." else "$running. $estimate."
            }
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
        // The headline is what the step says it is doing; with nothing said, the stage its log is at.
        if (s.optString("message").isEmpty()) findViewById<TextView>(R.id.message).text = stageText
        val nothing = s.optString("message").isEmpty() && stageText.isEmpty()
        findViewById<TextView>(R.id.message).visibility =
            if (failed || nothing || state == "done" || state == "cancelled") View.GONE else View.VISIBLE
        findViewById<TextView>(R.id.error).apply {
            visibility = if (failed) View.VISIBLE else View.GONE
            val error = s.optString("error").ifEmpty { "No details were recorded. See the step's log." }
            if (text.toString() != error) text = error
        }
        findViewById<Button>(R.id.copy_error).visibility = if (failed) View.VISIBLE else View.GONE
        val bar = findViewById<LinearProgressIndicator>(R.id.bar)
        val total = s.optLong("totalBytes")
        bar.isIndeterminate = state == "running" && total == 0L && !paused
        if (total > 0) bar.setProgressCompat((1000 * s.optLong("doneBytes") / total).toInt(), true)
        bar.visibility = if (state == "done" || state == "failed" || state == "cancelled") View.GONE else View.VISIBLE

        renderSteps(s)
        findViewById<Button>(R.id.launch).visibility = if (state == "done") View.VISIBLE else View.GONE
        val stopped = state == "failed" || state == "cancelled" || state == "interrupted"
        findViewById<Button>(R.id.retry).visibility = if (stopped) View.VISIBLE else View.GONE
        findViewById<Button>(R.id.retry).text = if (state == "failed") "Retry from the failed step" else "Resume"
        findViewById<Button>(R.id.cancel).visibility = if (state == "running") View.VISIBLE else View.GONE
        findViewById<Button>(R.id.pause).apply {
            visibility = if (state == "running") View.VISIBLE else View.GONE
            text = if (paused) "Resume" else "Pause"
        }
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
                    CreateLogActivity.open(this, s.optString("name"), st.getString("key"))
                }
                rows[st.getString("key")] = row
                container.addView(row)
            }
        }
        for (i in 0 until steps.length()) {
            val st = steps.getJSONObject(i)
            val row = rows[st.getString("key")] ?: continue
            val state = st.getString("state")
            val current = st.getString("key") == s.optString("step") && state == "running"
            row.findViewById<TextView>(R.id.mark).apply {
                text = when (state) {
                    "done" -> "✓"
                    "running" -> "▶"
                    "failed" -> "✕"
                    else -> "·"
                }
                setTextColor(
                    MaterialColors.getColor(
                        this,
                        when (state) {
                            "failed" -> androidx.appcompat.R.attr.colorError
                            "done", "running" -> androidx.appcompat.R.attr.colorPrimary
                            else -> com.google.android.material.R.attr.colorOnSurfaceVariant
                        },
                    ),
                )
            }
            row.findViewById<TextView>(R.id.step_title).apply {
                setTypeface(null, if (current || state == "failed") Typeface.BOLD else Typeface.NORMAL)
                alpha = if (state == "pending") PENDING_ALPHA else 1f
            }
        }
    }

    // ------------------------------------------------------------------ the live panel

    private fun objects(a: JSONArray?): List<JSONObject> =
        if (a == null) emptyList() else (0 until a.length()).map { a.getJSONObject(it) }

    private fun fact(label: String, value: String, warn: Boolean): JSONObject =
        JSONObject().put("label", label).put("value", value).put("warn", warn)

    /** Android's own numbers: the battery's temperature and its thermal status, in what it reports. */
    private fun temperatureFacts(): List<JSONObject> {
        val battery = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val tenths = battery?.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, Int.MIN_VALUE) ?: Int.MIN_VALUE
        val temp = if (tenths == Int.MIN_VALUE) {
            fact("Battery", "not reported", false)
        } else {
            fact("Battery", String.format(Locale.US, "%.1f °C", tenths / 10.0), tenths >= HOT_BATTERY_TENTHS)
        }
        val status = getSystemService(PowerManager::class.java).currentThermalStatus
        val names = arrayOf("normal", "light", "moderate", "severe", "critical", "emergency", "shutdown")
        val thermal = fact("Thermal state", names.getOrElse(status) { "unknown" }, status >= PowerManager.THERMAL_STATUS_MODERATE)
        return listOf(temp, thermal)
    }

    private fun showLive(live: JSONObject) {
        val facts = objects(live.optJSONArray("facts"))
        renderFacts(findViewById(R.id.facts), facts)
        stageText = facts.firstOrNull()?.optString("value").orEmpty()
        renderFacts(
            findViewById(R.id.machine),
            objects(live.optJSONArray("vm")) + objects(live.optJSONArray("phone")) + temperatureFacts(),
        )
        val times = live.optJSONObject("stepTimes") ?: return
        for ((key, row) in rows) {
            row.findViewById<TextView>(R.id.step_time).text = times.optString(key)
        }
    }

    private fun renderFacts(grid: GridLayout, facts: List<JSONObject>) {
        val shape = facts.joinToString("|") { "${it.optString("label")}:${it.optBoolean("wide")}" }
        if (grid.tag != shape) {
            grid.tag = shape
            grid.removeAllViews()
            for (f in facts) {
                val span = if (f.optBoolean("wide")) 2 else 1
                grid.addView(
                    TextView(this).apply { textSize = FACT_SP },
                    GridLayout.LayoutParams(
                        GridLayout.spec(GridLayout.UNDEFINED),
                        GridLayout.spec(GridLayout.UNDEFINED, span, 1f),
                    ).apply { width = 0 },
                )
            }
        }
        val dim = MaterialColors.getColor(grid, com.google.android.material.R.attr.colorOnSurfaceVariant)
        val alert = MaterialColors.getColor(grid, androidx.appcompat.R.attr.colorError)
        facts.forEachIndexed { i, f ->
            val text = SpannableStringBuilder(f.optString("label"))
            text.setSpan(ForegroundColorSpan(dim), 0, text.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            text.append("  ")
            val start = text.length
            text.append(f.optString("value"))
            if (f.optBoolean("warn")) {
                text.setSpan(ForegroundColorSpan(alert), start, text.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
                text.setSpan(StyleSpan(Typeface.BOLD), start, text.length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
            (grid.getChildAt(i) as TextView).text = text
        }
    }

    companion object {
        private const val MIB = 1024L * 1024
        private const val HOT_BATTERY_TENTHS = 430
        private const val FACT_SP = 12f
        private const val PENDING_ALPHA = 0.7f

        /** As the native side writes durations: "2 h 05 min", "12 min", "40 s". */
        fun durationText(secs: Long): String {
            val (h, m) = secs / 3600 to (secs % 3600) / 60
            return when {
                h > 0 -> String.format(Locale.US, "%d h %02d min", h, m)
                m > 0 -> "$m min"
                else -> "$secs s"
            }
        }

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
