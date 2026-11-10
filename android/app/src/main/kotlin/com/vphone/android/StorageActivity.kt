package com.vphone.android

import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.DialogAction
import com.vphone.android.ui.DialogHost
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.ProgressUi
import com.vphone.android.ui.StorageButton
import com.vphone.android.ui.StorageRow
import com.vphone.android.ui.StorageScreen
import com.vphone.android.ui.StorageSection
import com.vphone.android.ui.StorageUi
import com.vphone.android.ui.VphoneTheme
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.Executors

/** The Rust half of this screen, `src/create/storage.rs`. Everything crosses as JSON. */
object StorageNative {
    init {
        System.loadLibrary("vphone_native")
    }

    external fun scan(): String
    external fun plan(opJson: String): String
    external fun start(opJson: String): String
    external fun progress(): String
    external fun takeResult()
    external fun generation(): Long
}

/** What the app keeps on the phone, and giving it back: VMs, firmware, leftovers. */
class StorageActivity : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private val dialogs = DialogHost()

    /** The last measurement as the screen shows it; [busy] and [progress] are laid over it. */
    private var measured by mutableStateOf(StorageUi())
    private var progress by mutableStateOf<ProgressUi?>(null)
    private var running by mutableStateOf(false)
    private var scanning = false
    /** A VM the list's Delete sent here: its deletion is offered once the first measurement is back. */
    private var pendingDelete = ""

    private val poll = object : Runnable {
        override fun run() {
            follow()
            handler.postDelayed(this, POLL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) pendingDelete = intent.getStringExtra(EXTRA_DELETE).orEmpty()
        setContent {
            VphoneTheme {
                val m = measured
                StorageScreen(
                    StorageUi(m.summary, m.used, m.sections, m.resetText, m.reset, progress, running),
                    onBack = ::finish,
                )
                dialogs.current?.let { spec ->
                    AppDialog(spec, onCopy = { App.copy(this, it) }, onDismiss = { dialogs.current = null })
                }
            }
        }
    }

    override fun onStart() {
        super.onStart()
        refresh()
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

    // ------------------------------------------------------------------ measuring

    /** Walks every VM, so it runs off the main thread. */
    private fun refresh() {
        if (scanning) return
        scanning = true
        worker.execute {
            val scan = runCatching { JSONObject(StorageNative.scan()) }
            handler.post {
                scanning = false
                if (isDestroyed) return@post
                scan.onSuccess(::show).onFailure { showFailure("Could not measure storage", it.toString()) }
            }
        }
    }

    private fun show(s: JSONObject) {
        val total = s.getLong("totalBytes")
        val vms = objects(s.getJSONArray("vms"))
        val firmware = objects(s.getJSONArray("firmware"))
        val processes = objects(s.getJSONArray("processes"))
        val leftovers = objects(s.getJSONArray("leftovers"))
        val leftoverRows = ArrayList<StorageRow>()
        if (processes.isNotEmpty()) {
            val names = processes.joinToString(", ") { "${it.getString("name")} (${it.getInt("pid")})" }
            leftoverRows += StorageRow(
                "Helpers from a run that is over",
                "Still alive: $names. Stopping them only ends this app's own processes.",
                s.optString("processesBlocked"),
                listOf(action("Stop them", op("stopProcesses"), s.optString("processesBlocked").isEmpty())),
            )
        }
        leftovers.forEach { leftoverRows += leftoverRow(it) }
        val resetBlocked = s.optString("resetBlocked")
        measured = StorageUi(
            summary = "Phone: ${s.getString("freeSize")} free of ${s.getString("totalSize")}\n" +
                "This app uses ${s.getString("appSize")}: VMs ${s.getString("vmsSize")}, " +
                "firmware ${s.getString("firmwareSize")}, leftovers ${s.getString("leftoversSize")}",
            used = if (total > 0) (total - s.getLong("freeBytes")).toFloat() / total else 0f,
            sections = listOf(
                StorageSection("Virtual machines", "No VMs on this phone.", vms.map(::vmRow)),
                StorageSection(
                    "Firmware downloads",
                    "The firmware cache is empty.",
                    firmware.map(::firmwareRow),
                    if (firmware.isEmpty()) null else action("Delete all firmware", op("deleteAllFirmware"), firmware.any { it.optString("blocked").isEmpty() }, danger = true),
                ),
                StorageSection(
                    "Leftovers",
                    "Nothing is left over.",
                    leftoverRows,
                    if (leftovers.isEmpty()) null else action("Clear all leftovers", op("clearAllLeftovers"), leftovers.any { it.optString("blocked").isEmpty() }, danger = true),
                ),
            ),
            resetText = if (resetBlocked.isEmpty()) {
                "Removes every VM, the firmware cache and all leftovers (${s.getString("resetSize")} in total), " +
                    "and returns the app to its first-run state."
            } else {
                "Not available now: $resetBlocked"
            },
            reset = action("Reset everything…", op("resetEverything"), resetBlocked.isEmpty(), danger = true),
        )
        if (pendingDelete.isNotEmpty()) {
            val name = pendingDelete
            pendingDelete = ""
            val vm = vms.firstOrNull { it.getString("name") == name }
            when {
                vm == null -> showFailure("Cannot delete $name", "$name is not among the VMs on this phone.")
                vm.optBoolean("external") -> showFailure("Cannot delete $name", "$name is outside the app's own storage, and the app does not remove it.")
                else -> confirm(op("deleteVm", name))
            }
        }
    }

    private fun vmRow(vm: JSONObject): StorageRow {
        val name = vm.getString("name")
        val blocked = vm.optString("blocked")
        val device = listOf(vm.optString("device"), vm.optString("ios").let { if (it.isEmpty()) "" else "iOS $it" })
            .filter { it.isNotEmpty() }.joinToString(", ")
        val detail = buildString {
            if (device.isNotEmpty()) appendLine(device)
            appendLine(vm.getString("stateText"))
            append("${vm.getString("size")} on disk, ${vm.getString("logSize")} of it logs")
        }
        val buttons = ArrayList<StorageButton>()
        val logPath = vm.optString("log")
        if (logPath.isNotEmpty()) buttons += StorageButton("View log", true) { showLog(logPath) }
        if (vm.optBoolean("external")) return StorageRow(name, detail, blocked, buttons)
        buttons += action("Clear logs", op("clearVmLogs", name), blocked.isEmpty())
        buttons += action("Reset", op("resetVm", name), vm.optString("resetBlocked").isEmpty())
        buttons += action("Delete", op("deleteVm", name), blocked.isEmpty(), danger = true)
        val note = blocked.ifEmpty { vm.optString("resetBlocked").let { if (it.isEmpty()) "" else "Reset: $it" } }
        return StorageRow(name, detail, note, buttons)
    }

    private fun firmwareRow(f: JSONObject): StorageRow {
        val detail = buildString {
            append(f.getString("size"))
            if (f.optBoolean("partial")) append(", partial")
            f.optString("note").let { if (it.isNotEmpty()) append("\n$it") }
        }
        val blocked = f.optString("blocked")
        return StorageRow(
            f.getString("name"),
            detail,
            blocked,
            listOf(action("Delete", op("deleteFirmware", f.getString("name")), blocked.isEmpty(), danger = true)),
        )
    }

    private fun leftoverRow(l: JSONObject): StorageRow {
        val blocked = l.optString("blocked")
        return StorageRow(
            l.getString("title"),
            "${l.getString("size")}. ${l.optString("detail")}",
            blocked,
            listOf(action("Clear", op("clearLeftover", l.getString("id")), blocked.isEmpty())),
        )
    }

    /** The end of a log, whole and copyable. */
    private fun showLog(path: String) {
        worker.execute {
            val log = Native.logTail(path, 32768).ifEmpty { "(the log is empty)" }
            handler.post {
                if (isDestroyed) return@post
                dialogs.show(
                    DialogSpec(path.substringAfterLast('/'), log, confirm = DialogAction("Close"), copyText = log, monospace = true),
                )
            }
        }
    }

    // ------------------------------------------------------------------ actions

    private fun op(name: String, target: String = ""): JSONObject =
        JSONObject().put("op", name).put("name", target)

    private fun action(label: String, op: JSONObject, enabled: Boolean, danger: Boolean = false) =
        StorageButton(label, enabled, danger) { confirm(op) }

    /** Measured again for the confirmation, so what it states is what is on disk now. */
    private fun confirm(op: JSONObject) {
        if (running) return
        running = true
        worker.execute {
            val plan = runCatching { JSONObject(StorageNative.plan(op.toString())) }
            handler.post {
                running = false
                if (isDestroyed) return@post
                plan.onSuccess { showPlan(op, it) }.onFailure { showFailure("Could not prepare this", it.toString()) }
            }
        }
    }

    /** A plan with a word to type is the last gate before erasing everything. */
    private fun showPlan(op: JSONObject, plan: JSONObject) {
        val blocked = plan.optString("blocked")
        if (blocked.isNotEmpty()) {
            showFailure(plan.getString("title"), blocked)
            return
        }
        val message = plan.getString("text") + if (plan.optLong("bytes") > 0) "\n\nThis frees about ${plan.getString("size")}." else ""
        dialogs.show(
            DialogSpec(
                plan.getString("title"),
                message,
                confirm = DialogAction(plan.getString("action")) { begin(op) },
                dismiss = DialogAction("Cancel"),
                typeWord = plan.optString("confirmWord"),
            ),
        )
    }

    private fun begin(op: JSONObject) {
        val error = StorageNative.start(op.toString())
        if (error.isNotEmpty()) showFailure("Cannot start", error)
        follow()
    }

    // ------------------------------------------------------------------ progress

    private fun follow() {
        val p = runCatching { JSONObject(StorageNative.progress()) }.getOrNull() ?: return
        val active = p.optBoolean("active")
        if (active != running) running = active
        progress = if (active) {
            val total = p.optLong("totalBytes")
            val done = p.optLong("doneBytes")
            ProgressUi(
                title = p.optString("title"),
                detail = p.optString("current"),
                fraction = if (total > 0) (done.toFloat() / total).coerceAtMost(1f) else null,
                stats = if (total > 0) listOf("${gib(done)} of ${gib(total)}") else emptyList(),
            )
        } else {
            null
        }
        val result = p.optJSONObject("result") ?: return
        StorageNative.takeResult()
        running = false
        showResult(result)
        refresh()
    }

    private fun gib(bytes: Long): String = String.format(java.util.Locale.US, "%.2f GB", bytes / (1024.0 * 1024 * 1024))

    /** The result as text, whole and copyable, so a partial failure can be reported in full. */
    private fun showResult(r: JSONObject) {
        val report = r.getString("report")
        val title = when {
            r.optBoolean("ok") -> "Done: freed ${r.getString("freed")}"
            r.optBoolean("attempted") -> "Finished with problems"
            else -> "Nothing was removed"
        }
        dialogs.show(DialogSpec(title, report, confirm = DialogAction("Close"), copyText = report, isError = !r.optBoolean("ok")))
    }

    private fun showFailure(title: String, details: String) =
        dialogs.show(DialogSpec(title, details, confirm = DialogAction("Close"), copyText = details, isError = true))

    private fun objects(a: JSONArray): List<JSONObject> = (0 until a.length()).map { a.getJSONObject(it) }

    companion object {
        /** A VM's name: its deletion is planned and offered as soon as the screen has measured. */
        const val EXTRA_DELETE = "delete"
        private const val POLL_MS = 400L
    }
}
