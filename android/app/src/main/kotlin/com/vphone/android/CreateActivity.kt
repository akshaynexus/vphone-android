package com.vphone.android

import android.content.Intent
import android.content.IntentFilter
import android.net.Uri
import android.os.BatteryManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.CreatePickActions
import com.vphone.android.ui.CreatePickScreen
import com.vphone.android.ui.CreatePickUi
import com.vphone.android.ui.CreateProgressActions
import com.vphone.android.ui.CreateProgressScreen
import com.vphone.android.ui.CreateProgressUi
import com.vphone.android.ui.CreateStep
import com.vphone.android.ui.DialogAction
import com.vphone.android.ui.DialogHost
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.Fact
import com.vphone.android.ui.VphoneTheme
import org.json.JSONArray
import org.json.JSONObject
import java.util.Locale
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/** The wizard: pick a device and iOS version, then follow the creation to the running VM. */
class CreateActivity : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private val dialogs = DialogHost()
    /** The VM being followed; null while the pickers are shown. */
    private var following by mutableStateOf<String?>(null)
    private var pick by mutableStateOf(CreatePickUi())
    private var progress by mutableStateOf(CreateProgressUi())
    private var lastStatus: JSONObject? = null
    private var paused = false
    private lateinit var catalog: JSONObject
    private lateinit var deviceIds: List<String>
    private lateinit var pairingsByDevice: Map<String, List<JSONObject>>
    private var pickedDevice = ""
    private var pickedIos: JSONObject? = null
    private var stageText = ""
    private var stepTimes: JSONObject? = null
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
                    live?.let(::showLive)
                    status?.let(::showStatus)
                }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val pending = Native.createPending()
        if (pending.isNotEmpty()) follow(pending) else showPicker()
        val pickActions = CreatePickActions(
            device = { i -> selectDevice(deviceIds[i]) },
            version = { i -> selectIos(pairingsByDevice.getValue(pickedDevice)[i]) },
            name = { edit { copy(name = it) } },
            scale = { edit { copy(scale = it) } },
            cellular = { edit { copy(cellular = it) } },
            deleteFirmware = { edit { copy(deleteFirmware = it) } },
            trimServices = { edit { copy(trimServices = it) } },
            start = ::confirmAndStart,
            importInstead = {
                startActivity(Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_IMPORT, true))
                finish()
            },
            back = ::finish,
        )
        val progressActions = CreateProgressActions(
            copyError = ::copyReport,
            liveLog = { following?.let { CreateLogActivity.open(this, it) } },
            watchVm = { startActivity(Intent(this, CreateVmScreen::class.java)) },
            pause = { following?.let { if (paused) Native.createPause(it, false) else confirmPause(it) } },
            cancel = { following?.let(::confirmCancel) },
            delete = { following?.let(::confirmDelete) },
            retry = { following?.let { CreateService.start(this, it) } },
            useCellular = {
                following?.let { name ->
                    dialogs.show(
                        DialogSpec(
                            "Download over cellular?",
                            "The remaining firmware (up to about 12 GB) would use your mobile data.",
                            confirm = DialogAction("Use cellular") { Native.createAllowCellular(name, true) },
                            dismiss = DialogAction("Wait for Wi-Fi"),
                        ),
                    )
                }
            },
            launch = {
                following?.let { name ->
                    startActivity(Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_BOOT, name))
                    finish()
                }
            },
            stepLog = { key -> following?.let { CreateLogActivity.open(this, it, key) } },
            back = ::finish,
        )
        setContent {
            VphoneTheme {
                if (following == null) CreatePickScreen(pick, pickActions) else CreateProgressScreen(progress, progressActions)
                dialogs.current?.let { spec ->
                    AppDialog(spec, onCopy = { App.copy(this, it) }, onDismiss = { dialogs.current = null })
                }
            }
        }
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

    /** A change to what is picked, and what the native side says of the result. */
    private fun edit(change: CreatePickUi.() -> CreatePickUi) {
        pick = pick.change()
        updatePreflight()
    }

    private fun showPicker() {
        following = null
        catalog = JSONObject(Native.createCatalog())
        val devices = catalog.getJSONArray("devices")
        val all = (0 until devices.length()).map { devices.getJSONObject(it) }
        deviceIds = all.map { it.getString("id") }
        pairingsByDevice = all.associate { d ->
            val p = d.getJSONArray("pairings")
            d.getString("id") to (0 until p.length()).map { p.getJSONObject(it) }
        }
        pick = CreatePickUi(
            about = ABOUT,
            trimNote = TRIM_NOTE,
            devices = all.map { "${it.getString("name")} (${it.getString("id")})" },
        )
        val def = catalog.getString("defaultDevice")
        selectDevice(deviceIds[deviceIds.indexOf(def).coerceAtLeast(0)])
        pairingsByDevice.getValue(pickedDevice)
            .firstOrNull { it.getString("ios") == catalog.getString("defaultIos") }
            ?.let(::selectIos)
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
        pick = pick.copy(device = deviceIds.indexOf(id), versions = pairings.map { labelFor(it) })
        val first = pairings.firstOrNull { it.optBoolean("recommended") }
            ?: pairings.firstOrNull { it.getString("status") == "tested" }
            ?: pairings.first()
        selectIos(first)
    }

    private fun selectIos(p: JSONObject) {
        pickedIos = p
        val underTest = p.getString("status") != "tested"
        val minScale = p.optInt("minScale", 1)
        pick = pick.copy(
            version = pairingsByDevice.getValue(pickedDevice).indexOf(p).coerceAtLeast(0),
            pairingNote = p.optString("note"),
            warning = if (underTest) {
                "Under test: this pairing has been verified on macOS only, not with the Linux flow this app follows. " +
                    "Expect it may need manual attention, and hours of work to be lost if it fails."
            } else {
                ""
            },
            minScale = minScale,
            scale = if (minScale <= 1) 1 else 2,
            name = "vphone-" + pickedDevice.replace(",", "-") + "-" + p.getString("ios").replace(".", ""),
        )
        updatePreflight()
    }

    private fun specJson(): JSONObject {
        val p = pickedIos!!
        return JSONObject()
            .put("name", pick.name)
            .put("device", pickedDevice)
            .put("ios", p.getString("ios"))
            .put("build", p.getString("build"))
            .put("scale", pick.scale)
            .put("allowCellular", pick.cellular)
            .put("deleteFirmwareAfter", pick.deleteFirmware)
            .put("keepAllServices", !pick.trimServices)
    }

    private fun updatePreflight(): JSONObject {
        val report = JSONObject(Native.createPreflight(specJson().toString()))
        val errors = report.getJSONArray("errors")
        val warnings = report.getJSONArray("warnings")
        pick = pick.copy(
            estimate = report.optString("estimate"),
            storage = "Storage free: ${report.optLong("freeGb")} GB (about ${report.optLong("wantGb")} GB will be used).",
            errors = (0 until errors.length()).map { errors.getString(it) },
            warnings = (0 until warnings.length()).map { warnings.getString(it) },
        )
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
        dialogs.show(
            DialogSpec(
                "Create ${spec.getString("name")}?",
                "$TIME_WARNING$battery",
                confirm = DialogAction("Start") { maybeAskBattery(spec) },
                dismiss = DialogAction("Back"),
            ),
        )
    }

    /** Without this, Samsung's and OnePlus's battery managers stop the app in the background. */
    private fun maybeAskBattery(spec: JSONObject) {
        val power = getSystemService(PowerManager::class.java)
        if (power.isIgnoringBatteryOptimizations(packageName)) {
            start(spec)
            return
        }
        dialogs.show(
            DialogSpec(
                "Keep it running with the screen off",
                "Without unrestricted battery use, this phone's battery manager can stop creation when the app is not on screen. Allow it for vphone.",
                confirm = DialogAction("Allow") {
                    startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
                    start(spec)
                },
                dismiss = DialogAction("Not now") { start(spec) },
            ),
        )
    }

    private fun start(spec: JSONObject) {
        val error = Native.createBegin(spec.toString())
        if (error.isNotEmpty()) {
            dialogs.show(DialogSpec.error("Cannot start", error))
            return
        }
        val name = spec.getString("name")
        CreateService.start(this, name)
        follow(name)
    }

    // ------------------------------------------------------------------ following one

    private fun follow(name: String) {
        progress = CreateProgressUi(name = name, nowStep = "Getting ready", busy = true)
        stageText = ""
        stepTimes = null
        following = name
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
        dialogs.show(
            DialogSpec(
                "Pause now?",
                message,
                confirm = DialogAction("Pause") { Native.createPause(name, true) },
                dismiss = DialogAction("Keep running"),
            ),
        )
    }

    private fun confirmCancel(name: String) {
        dialogs.show(
            DialogSpec(
                "Stop creating this VM?",
                "It ends the current step. You can resume later from the start of that step. To carry on without losing the step, use Pause instead.",
                confirm = DialogAction("Stop it") { Native.createCancel(name) },
                dismiss = DialogAction("Keep going"),
            ),
        )
    }

    private fun confirmDelete(name: String) {
        dialogs.show(
            DialogSpec(
                "Delete $name?",
                confirm = DialogAction("Delete"),
                dismiss = DialogAction("Keep it"),
                checkLabel = "Also delete the cached firmware",
                onConfirmChecked = { alsoFirmware ->
                    Native.createCancel(name)
                    val error = Native.createDelete(name, alsoFirmware)
                    // A failure is shown whole, and the screen stays so it can be read.
                    if (error.isNotEmpty()) dialogs.show(DialogSpec.error("$name was not deleted", error)) else finish()
                },
            ),
        )
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

    private fun showStatus(s: JSONObject) {
        lastStatus = s
        val state = s.optString("state")
        paused = s.optBoolean("paused")
        val failed = state == "failed"
        val all = s.optJSONArray("steps")
        val stepList = (0 until (all?.length() ?: 0)).map { all!!.getJSONObject(it) }
        val at = stepList.indexOfFirst { it.optString("key") == s.optString("step") }
        // The time left is only ever made of times this phone measured; without them, the reason.
        val left = s.optLong("etaSecs")
        val estimate = when {
            left > 0 -> "About ${durationText(left)} left, going by this phone's last run of these steps"
            else -> s.optString("etaNote")
        }
        val holdKey = s.optString("hold")
        val hot = s.optInt("thermal")
        // The headline is what the step says it is doing; with nothing said, the stage its log is at.
        val said = s.optString("message")
        val speed = s.optLong("bytesPerSec")
        val message = when {
            failed || state == "done" || state == "cancelled" -> ""
            said.isEmpty() -> stageText
            speed > 0 -> "$said (${speed / MIB} MB/s)"
            else -> said
        }
        val total = s.optLong("totalBytes")
        val stopped = state == "failed" || state == "cancelled" || state == "interrupted"
        progress = CreateProgressUi(
            name = s.optString("name"),
            subtitle = "${s.optString("device")}, iOS ${s.optString("ios")}",
            nowStep = when {
                state == "done" -> "All ${stepList.size} steps finished"
                at >= 0 -> "Step ${at + 1} of ${stepList.size} \u00b7 ${stepList[at].optString("title")}"
                else -> "Getting ready"
            },
            eta = when (state) {
                "done" -> "Finished."
                "failed" -> "Stopped with an error."
                "cancelled" -> "Stopped. Resume it or delete it."
                "interrupted" -> if (paused) "Paused. Tap Resume to carry on." else "Interrupted. Tap Resume to continue."
                else -> {
                    val running = if (paused) "Paused: tap Resume to carry on" else "Running"
                    if (estimate.isEmpty()) "$running." else "$running. $estimate."
                }
            },
            hold = when (holdKey) {
                "wifi" -> "On hold: waiting for Wi-Fi."
                "network" -> "On hold: waiting for a connection."
                "storage" -> "On hold: storage is low. $said"
                "thermal" -> "On hold: the phone is cooling down."
                else -> ""
            },
            showCellular = holdKey == "wifi",
            thermal = when {
                hot < 2 -> ""
                hot == 2 -> "The phone is warm; this may be slower."
                hot == 3 -> "The phone is hot; this will be slower."
                else -> "The phone is overheating: let it cool down."
            },
            message = message,
            error = if (failed) s.optString("error").ifEmpty { "No details were recorded. See the step's log." } else null,
            barVisible = !(state == "done" || state == "failed" || state == "cancelled"),
            fraction = if (total > 0) s.optLong("doneBytes").toFloat() / total else null,
            busy = state == "running" && !paused,
            steps = stepList.map { st ->
                val key = st.getString("key")
                val stepState = st.getString("state")
                CreateStep(key, st.getString("title"), stepState, key == s.optString("step") && stepState == "running", stepTimes?.optString(key).orEmpty())
            },
            phases = progress.phases,
            facts = progress.facts,
            machine = progress.machine,
            showLaunch = state == "done",
            retryLabel = if (!stopped) null else if (failed) "Retry from the failed step" else "Resume",
            showCancel = state == "running",
            pauseLabel = if (state != "running") null else if (paused) "Resume" else "Pause",
        )
    }

    // ------------------------------------------------------------------ the live panel

    private fun objects(a: JSONArray?): List<JSONObject> =
        if (a == null) emptyList() else (0 until a.length()).map { a.getJSONObject(it) }

    private fun facts(a: JSONArray?): List<Fact> =
        objects(a).map { Fact(it.optString("label"), it.optString("value"), it.optBoolean("warn"), it.optBoolean("wide")) }

    /** Android's own numbers: the battery's temperature and its thermal status, in what it reports. */
    private fun temperatureFacts(): List<Fact> {
        val battery = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val tenths = battery?.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, Int.MIN_VALUE) ?: Int.MIN_VALUE
        val temp = if (tenths == Int.MIN_VALUE) {
            Fact("Battery", "not reported")
        } else {
            Fact("Battery", String.format(Locale.US, "%.1f °C", tenths / 10.0), tenths >= HOT_BATTERY_TENTHS)
        }
        val status = getSystemService(PowerManager::class.java).currentThermalStatus
        val names = arrayOf("normal", "light", "moderate", "severe", "critical", "emergency", "shutdown")
        val thermal = Fact("Thermal state", names.getOrElse(status) { "unknown" }, status >= PowerManager.THERMAL_STATUS_MODERATE)
        return listOf(temp, thermal)
    }

    private fun showLive(live: JSONObject) {
        val stepFacts = facts(live.optJSONArray("facts"))
        stageText = stepFacts.firstOrNull()?.value.orEmpty()
        // The first boot's phases: what each took, how long the running one has run, and what it
        // last took on this phone, or that there is no estimate yet.
        val phases = live.optJSONArray("phases")
        stepTimes = live.optJSONObject("stepTimes") ?: stepTimes
        progress = progress.copy(
            facts = stepFacts,
            machine = facts(live.optJSONArray("vm")) + facts(live.optJSONArray("phone")) + temperatureFacts(),
            phases = (0 until (phases?.length() ?: 0)).map { phases!!.getString(it) },
        )
    }

    companion object {
        private const val MIB = 1024L * 1024
        private const val HOT_BATTERY_TENTHS = 430

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

        private const val TRIM_NOTE =
            "On: during the first boot about 137 of iOS's background services are disabled, which a VM " +
                "has no use for: Siri and the intelligence services, Find My, Wallet's NFC, Watch and Home, " +
                "health coaching, Maps' background work, iCloud, iMessage and Mail sync, ads and " +
                "recommendations, analytics, photo and media analysis, accessory updates, software update, " +
                "and the App Store's services. The VM then runs fewer processes, uses less memory and writes " +
                "less. They stay off from the VM's next start; those already running in the first boot keep " +
                "running until then. Leave it off if you need Siri, dictation, Mail, iMessage, HomeKit, the " +
                "App Store or the Weather widget in this VM."

        private const val ABOUT =
            "Builds a VM from Apple's firmware with no computer and no root. The pairing marked tested has been " +
                "verified end to end; others may need extra work.\n\n" +
                "You need about 60 GB free while it runs (the finished VM takes 20 to 25 GB) and 8 GB of RAM or more. " +
                "Firmware is reused for later VMs unless you choose to delete it."
    }
}
