package com.vphone.android

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.ColorStateList
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton
import com.google.android.material.card.MaterialCardView
import com.google.android.material.color.MaterialColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.progressindicator.LinearProgressIndicator
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
class StorageActivity : AppCompatActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var summary: TextView
    private lateinit var summaryBar: LinearProgressIndicator
    private lateinit var progressCard: View
    private lateinit var progressTitle: TextView
    private lateinit var progressDetail: TextView
    private lateinit var progressBar: LinearProgressIndicator
    private lateinit var vmSection: LinearLayout
    private lateinit var firmwareSection: LinearLayout
    private lateinit var leftoverSection: LinearLayout
    private lateinit var resetSection: LinearLayout
    private val actions = ArrayList<Pair<Button, Boolean>>()
    private var running = false
    private var scanning = false

    private val poll = object : Runnable {
        override fun run() {
            follow()
            handler.postDelayed(this, POLL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        summary = body()
        summaryBar = LinearProgressIndicator(this).apply {
            max = 1000
            setPadding(0, dp(8), 0, 0)
        }
        progressTitle = text(com.google.android.material.R.style.TextAppearance_Material3_TitleMedium)
        progressDetail = body()
        progressBar = LinearProgressIndicator(this).apply {
            max = 1000
            setPadding(0, dp(8), 0, 0)
        }
        progressCard = card(listOf(progressTitle, progressDetail, progressBar)).apply { visibility = View.GONE }
        vmSection = column()
        firmwareSection = column()
        leftoverSection = column()
        resetSection = column()
        val content = column().apply {
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(text(com.google.android.material.R.style.TextAppearance_Material3_HeadlineMedium).apply { text = "Storage" })
            addView(card(listOf(summary, summaryBar)))
            addView(progressCard)
            addView(vmSection)
            addView(firmwareSection)
            addView(leftoverSection)
            addView(resetSection)
        }
        val scroll = ScrollView(this).apply { addView(content) }
        setContentView(scroll)
        App.fitSystemBars(scroll)
        summary.text = "Measuring…"
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

    @SuppressLint("SetTextI18n")
    private fun show(s: JSONObject) {
        actions.clear()
        summary.text = "Phone: ${s.getString("freeSize")} free of ${s.getString("totalSize")}\n" +
            "This app uses ${s.getString("appSize")}: VMs ${s.getString("vmsSize")}, " +
            "firmware ${s.getString("firmwareSize")}, leftovers ${s.getString("leftoversSize")}"
        val total = s.getLong("totalBytes")
        summaryBar.progress = if (total > 0) (1000 * (total - s.getLong("freeBytes")) / total).toInt() else 0

        vmSection.removeAllViews()
        vmSection.addView(heading("Virtual machines"))
        val vms = objects(s.getJSONArray("vms"))
        if (vms.isEmpty()) vmSection.addView(body().apply { text = "No VMs on this phone." })
        vms.forEach { vmSection.addView(vmRow(it)) }

        firmwareSection.removeAllViews()
        firmwareSection.addView(heading("Firmware downloads"))
        val firmware = objects(s.getJSONArray("firmware"))
        if (firmware.isEmpty()) {
            firmwareSection.addView(body().apply { text = "The firmware cache is empty." })
        } else {
            firmwareSection.addView(wideAction("Delete all firmware", op("deleteAllFirmware"), firmware.any { it.optString("blocked").isEmpty() }))
        }
        firmware.forEach { firmwareSection.addView(firmwareRow(it)) }

        leftoverSection.removeAllViews()
        leftoverSection.addView(heading("Leftovers"))
        val processes = objects(s.getJSONArray("processes"))
        if (processes.isNotEmpty()) {
            val names = processes.joinToString(", ") { "${it.getString("name")} (${it.getInt("pid")})" }
            leftoverSection.addView(
                row(
                    "Helpers from a run that is over",
                    "Still alive: $names. Stopping them only ends this app's own processes.",
                    s.optString("processesBlocked"),
                    actionsRow(action("Stop them", op("stopProcesses"), s.optString("processesBlocked").isEmpty())),
                ),
            )
        }
        val leftovers = objects(s.getJSONArray("leftovers"))
        if (leftovers.isEmpty() && processes.isEmpty()) {
            leftoverSection.addView(body().apply { text = "Nothing is left over." })
        }
        if (leftovers.isNotEmpty()) {
            leftoverSection.addView(wideAction("Clear all leftovers", op("clearAllLeftovers"), leftovers.any { it.optString("blocked").isEmpty() }))
        }
        leftovers.forEach { leftoverSection.addView(leftoverRow(it)) }

        resetSection.removeAllViews()
        resetSection.addView(resetCard(s))
        applyEnabled()
    }

    private fun vmRow(vm: JSONObject): View {
        val name = vm.getString("name")
        val blocked = vm.optString("blocked")
        val device = listOf(vm.optString("device"), vm.optString("ios").let { if (it.isEmpty()) "" else "iOS $it" })
            .filter { it.isNotEmpty() }.joinToString(", ")
        val detail = buildString {
            if (device.isNotEmpty()) appendLine(device)
            appendLine(vm.getString("stateText"))
            append("${vm.getString("size")} on disk, ${vm.getString("logSize")} of it logs")
        }
        val rows = column()
        val logPath = vm.optString("log")
        if (logPath.isNotEmpty()) {
            rows.addView(actionsRow(plainButton("View log") { App.showLog(this, logPath) }))
        }
        if (vm.optBoolean("external")) return row(name, detail, blocked, rows)
        rows.addView(
            actionsRow(
                action("Clear logs", op("clearVmLogs", name), blocked.isEmpty()),
                action("Reset", op("resetVm", name), vm.optString("resetBlocked").isEmpty()),
                action("Delete", op("deleteVm", name), blocked.isEmpty(), danger = true),
            ),
        )
        val note = blocked.ifEmpty { vm.optString("resetBlocked").let { if (it.isEmpty()) "" else "Reset: $it" } }
        return row(name, detail, note, rows)
    }

    private fun firmwareRow(f: JSONObject): View {
        val detail = buildString {
            append(f.getString("size"))
            if (f.optBoolean("partial")) append(", partial")
            f.optString("note").let { if (it.isNotEmpty()) append("\n$it") }
        }
        val blocked = f.optString("blocked")
        return row(
            f.getString("name"),
            detail,
            blocked,
            actionsRow(action("Delete", op("deleteFirmware", f.getString("name")), blocked.isEmpty(), danger = true)),
        )
    }

    private fun leftoverRow(l: JSONObject): View {
        val blocked = l.optString("blocked")
        return row(
            l.getString("title"),
            "${l.getString("size")}. ${l.optString("detail")}",
            blocked,
            actionsRow(action("Clear", op("clearLeftover", l.getString("id")), blocked.isEmpty())),
        )
    }

    private fun resetCard(s: JSONObject): View {
        val blocked = s.optString("resetBlocked")
        val button = action("Reset everything…", op("resetEverything"), blocked.isEmpty(), danger = true)
        val error = MaterialColors.getColor(summary, androidx.appcompat.R.attr.colorError)
        val message = text(com.google.android.material.R.style.TextAppearance_Material3_BodyMedium).apply {
            text = if (blocked.isEmpty()) {
                "Removes every VM, the firmware cache and all leftovers (${s.getString("resetSize")} in total), " +
                    "and returns the app to its first-run state."
            } else {
                "Not available now: $blocked"
            }
            setTextColor(error)
        }
        return column().apply {
            setPadding(0, dp(28), 0, 0)
            addView(card(listOf(heading("Reset everything").apply { setTextColor(error) }, message, button), strokeColor = error))
        }
    }

    // ------------------------------------------------------------------ actions

    private fun op(name: String, target: String = ""): JSONObject =
        JSONObject().put("op", name).put("name", target)

    /** Measured again for the confirmation, so what it states is what is on disk now. */
    private fun confirm(op: JSONObject) {
        if (running) return
        running = true
        applyEnabled()
        worker.execute {
            val plan = runCatching { JSONObject(StorageNative.plan(op.toString())) }
            handler.post {
                running = false
                applyEnabled()
                if (isDestroyed) return@post
                plan.onSuccess { showPlan(op, it) }.onFailure { showFailure("Could not prepare this", it.toString()) }
            }
        }
    }

    private fun showPlan(op: JSONObject, plan: JSONObject) {
        val blocked = plan.optString("blocked")
        if (blocked.isNotEmpty()) {
            showFailure(plan.getString("title"), blocked)
            return
        }
        val word = plan.optString("confirmWord")
        val message = plan.getString("text") + if (plan.optLong("bytes") > 0) "\n\nThis frees about ${plan.getString("size")}." else ""
        if (word.isEmpty()) {
            MaterialAlertDialogBuilder(this)
                .setTitle(plan.getString("title"))
                .setMessage(message)
                .setPositiveButton(plan.getString("action")) { _, _ -> begin(op) }
                .setNegativeButton("Cancel", null)
                .show()
            return
        }
        confirmByWord(op, plan.getString("title"), message, plan.getString("action"), word)
    }

    /** The last gate before erasing everything: the person types the word. */
    private fun confirmByWord(op: JSONObject, title: String, message: String, action: String, word: String) {
        val field = EditText(this).apply {
            hint = "Type $word"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS
        }
        val form = column().apply {
            setPadding(dp(24), dp(8), dp(24), 0)
            addView(body().apply { text = message })
            addView(TextView(context).apply { text = "\nType $word to continue." })
            addView(field)
        }
        val dialog: AlertDialog = MaterialAlertDialogBuilder(this)
            .setTitle(title)
            .setView(ScrollView(this).apply { addView(form) })
            .setPositiveButton(action) { _, _ -> begin(op) }
            .setNegativeButton("Cancel", null)
            .show()
        val go = dialog.getButton(AlertDialog.BUTTON_POSITIVE)
        go.isEnabled = false
        field.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) = Unit
            override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) = Unit
            override fun afterTextChanged(s: Editable?) {
                go.isEnabled = s?.toString() == word
            }
        })
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
        if (active != running) {
            running = active
            applyEnabled()
        }
        progressCard.visibility = if (active) View.VISIBLE else View.GONE
        if (active) {
            progressTitle.text = p.optString("title")
            val total = p.optLong("totalBytes")
            val done = p.optLong("doneBytes")
            progressBar.isIndeterminate = total <= 0
            if (total > 0) progressBar.setProgressCompat(((1000 * done) / total).toInt().coerceAtMost(1000), true)
            progressDetail.text = buildString {
                if (total > 0) append("${gib(done)} of ${gib(total)}. ")
                append(p.optString("current"))
            }
        }
        val result = p.optJSONObject("result") ?: return
        StorageNative.takeResult()
        running = false
        applyEnabled()
        showResult(result)
        refresh()
    }

    private fun applyEnabled() {
        actions.forEach { (button, allowed) -> button.isEnabled = allowed && !running }
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
        showReport(title, report)
    }

    private fun showFailure(title: String, details: String) = showReport(title, details)

    private fun showReport(title: String, text: String) {
        val view = TextView(this).apply {
            setPadding(dp(24), dp(8), dp(24), 0)
            setTextIsSelectable(true)
            this.text = text
        }
        MaterialAlertDialogBuilder(this)
            .setTitle(title)
            .setView(ScrollView(this).apply { addView(view) })
            .setNeutralButton("Copy") { _, _ -> App.copy(this, text) }
            .setPositiveButton("Close", null)
            .show()
    }

    // ------------------------------------------------------------------ building rows

    private fun objects(a: JSONArray): List<JSONObject> = (0 until a.length()).map { a.getJSONObject(it) }

    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

    private fun column(context: Context = this): LinearLayout = LinearLayout(context).apply { orientation = LinearLayout.VERTICAL }

    private fun text(style: Int): TextView = TextView(this).apply { setTextAppearance(style) }

    private fun body(): TextView = text(com.google.android.material.R.style.TextAppearance_Material3_BodyMedium)

    private fun heading(label: String): TextView =
        text(com.google.android.material.R.style.TextAppearance_Material3_TitleMedium).apply {
            setPadding(0, dp(20), 0, dp(4))
            text = label
        }

    private fun action(label: String, op: JSONObject, enabled: Boolean, danger: Boolean = false): Button {
        val button = plainButton(label) { confirm(op) }
        if (danger) {
            val error = MaterialColors.getColor(summary, androidx.appcompat.R.attr.colorError)
            val off = MaterialColors.compositeARGBWithAlpha(MaterialColors.getColor(summary, com.google.android.material.R.attr.colorOnSurface), 97)
            (button as MaterialButton).setTextColor(
                ColorStateList(arrayOf(intArrayOf(android.R.attr.state_enabled), intArrayOf()), intArrayOf(error, off)),
            )
        }
        actions += button to enabled
        return button
    }

    private fun plainButton(label: String, onTap: () -> Unit): Button =
        MaterialButton(this, null, com.google.android.material.R.attr.borderlessButtonStyle).apply {
            text = label
            isAllCaps = false
            setPadding(dp(8), 0, dp(8), 0)
            minWidth = dp(48)
            setOnClickListener { onTap() }
        }

    private fun wideAction(label: String, op: JSONObject, enabled: Boolean): View =
        actionsRow(action(label, op, enabled, danger = true))

    private fun actionsRow(vararg buttons: View): View = LinearLayout(this).apply {
        gravity = Gravity.END
        buttons.forEach { addView(it, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT)) }
    }

    private fun row(title: String, detail: String, blocked: String, buttons: View): View {
        val views = ArrayList<View>()
        views += text(com.google.android.material.R.style.TextAppearance_Material3_TitleSmall).apply { text = title }
        views += body().apply { text = detail }
        if (blocked.isNotEmpty()) {
            views += text(com.google.android.material.R.style.TextAppearance_Material3_BodySmall).apply {
                text = blocked
                setTextColor(MaterialColors.getColor(summary, androidx.appcompat.R.attr.colorError))
            }
        }
        views += buttons
        return card(views)
    }

    private fun card(children: List<View>, strokeColor: Int? = null): View {
        val inner = column().apply {
            setPadding(dp(16), dp(12), dp(16), dp(8))
            children.forEach { addView(it) }
        }
        return MaterialCardView(this, null, com.google.android.material.R.attr.materialCardViewOutlinedStyle).apply {
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
                topMargin = dp(10)
            }
            strokeColor?.let { setStrokeColor(it) }
            addView(inner)
        }
    }

    private companion object {
        const val POLL_MS = 400L
    }
}
