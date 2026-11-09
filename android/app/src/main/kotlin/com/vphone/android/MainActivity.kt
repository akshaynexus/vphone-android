package com.vphone.android

import android.Manifest
import android.app.Activity
import android.app.ActivityManager
import android.app.AlertDialog
import android.app.ApplicationExitInfo
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.text.InputType
import android.view.View
import android.view.WindowInsets
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import org.json.JSONObject

/** The VMs in app-specific storage, importing one, and what happened to the last run. */
class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var banner: TextView
    private lateinit var active: Button
    private lateinit var importButton: Button
    private lateinit var resumeCreate: Button
    private lateinit var importRow: LinearLayout
    private lateinit var importBar: ProgressBar
    private lateinit var importLabel: TextView
    private lateinit var list: LinearLayout
    private var importWasActive = false
    private var lastRunLog = ""
    private var storageGeneration = 0L

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        banner = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0xFF7A1F1F.toInt())
            setPadding(32, 24, 32, 24)
            visibility = View.GONE
            setOnClickListener { if (lastRunLog.isNotEmpty()) App.showLog(this@MainActivity, lastRunLog) }
        }
        active = Button(this).apply {
            isAllCaps = false
            visibility = View.GONE
            setOnClickListener { startActivity(Intent(this@MainActivity, VmActivity::class.java)) }
        }
        importButton = Button(this).apply {
            text = "Import a VM folder"
            isAllCaps = false
            setOnClickListener { pickFolder() }
        }
        val createButton = Button(this).apply {
            text = "Create a VM on this phone"
            isAllCaps = false
            setOnClickListener { startActivity(Intent(this@MainActivity, CreateActivity::class.java)) }
        }
        resumeCreate = Button(this).apply {
            isAllCaps = false
            visibility = View.GONE
            setOnClickListener { startActivity(Intent(this@MainActivity, CreateActivity::class.java)) }
        }
        importLabel = TextView(this)
        importBar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { max = 1000 }
        importRow = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            addView(importLabel)
            addView(importBar)
            addView(Button(this@MainActivity).apply {
                text = "Cancel import"
                isAllCaps = false
                setOnClickListener { VmService.cancelImport() }
            })
        }
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val info = JSONObject(Native.deviceInfo())
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(TextView(this@MainActivity).apply {
                text = "vphone"
                textSize = 28f
            })
            addView(TextView(this@MainActivity).apply {
                text = "${info.getInt("cpus")} CPU cores, ${info.getLong("totalMib") / 1024} GiB RAM, ${info.getLong("availableMib") / 1024} GiB free"
            })
            addView(banner)
            addView(active)
            addView(resumeCreate)
            addView(createButton)
            addView(importButton)
            addView(Button(this@MainActivity).apply {
                text = "Storage"
                isAllCaps = false
                setOnClickListener { startActivity(Intent(this@MainActivity, StorageActivity::class.java)) }
            })
            addView(importRow)
            addView(list)
        }
        val scroll = ScrollView(this).apply {
            addView(content)
            setOnApplyWindowInsetsListener { v, insets ->
                val bars = insets.getInsets(WindowInsets.Type.systemBars())
                v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
                insets
            }
        }
        setContentView(scroll)
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
        handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    /** The wizard's "Launch" and "import instead" arrive here. */
    private fun handleIntent(intent: Intent) {
        intent.getStringExtra(EXTRA_BOOT)?.let { name ->
            intent.removeExtra(EXTRA_BOOT)
            tryBoot(name)
        }
        if (intent.getBooleanExtra(EXTRA_IMPORT, false)) {
            intent.removeExtra(EXTRA_IMPORT)
            pickFolder()
        }
    }

    override fun onStart() {
        super.onStart()
        noteDeadProcess()
        showVms()
        handler.post(poll)
    }

    override fun onStop() {
        handler.removeCallbacks(poll)
        super.onStop()
    }

    /** A run still marked "running" with no VM in this process: find out what ended it. */
    private fun noteDeadProcess() {
        val last = Native.lastRun()
        if (last.isEmpty() || App.state().getString("phase") != "idle") return
        if (JSONObject(last).optString("phase") != "running") return
        val am = getSystemService(ActivityManager::class.java)
        val info = am.getHistoricalProcessExitReasons(packageName, 0, 1).firstOrNull()
        Native.recordDeath(info?.let { describeExit(it) } ?: "The app's process ended while the VM was running.")
    }

    private fun describeExit(info: ApplicationExitInfo): String {
        val description = info.description.orEmpty()
        val why = when {
            description.contains("MemoryLimiter") ->
                "Android's per-app memory limit ended the VM. Lower its RAM or code cache."
            info.reason == ApplicationExitInfo.REASON_LOW_MEMORY ->
                "Android ran out of memory and ended the VM. Lower its RAM, or close other apps."
            info.reason == ApplicationExitInfo.REASON_CRASH_NATIVE ->
                "QEMU or the GPU driver crashed (signal ${info.status})."
            info.reason == ApplicationExitInfo.REASON_SIGNALED ->
                "The process was killed by signal ${info.status}."
            info.reason == ApplicationExitInfo.REASON_EXIT_SELF ->
                "QEMU ended the process with status ${info.status}."
            info.reason == ApplicationExitInfo.REASON_USER_REQUESTED ->
                "The app was force-stopped."
            info.reason == ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE ->
                "Android stopped the app for excessive resource use."
            else -> "Android ended the process (reason ${info.reason})."
        }
        return if (description.isEmpty()) why else "$why ($description)"
    }

    private fun refresh() {
        val cleaned = StorageNative.generation()
        if (cleaned != storageGeneration) {
            storageGeneration = cleaned
            showVms()
        }
        val creating = Native.createPending()
        resumeCreate.visibility = if (creating.isEmpty()) View.GONE else View.VISIBLE
        if (creating.isNotEmpty()) resumeCreate.text = "Creating $creating: continue"
        val state = App.state()
        val phase = state.getString("phase")
        val vmUp = phase in setOf("preparing", "booting", "unlocking", "running", "stopping")
        active.visibility = if (vmUp) View.VISIBLE else View.GONE
        active.text = "Return to ${state.optString("vm")} (${VmService.phaseText(phase)})"

        val progress = JSONObject(Native.importProgress())
        val importing = progress.optBoolean("active")
        importRow.visibility = if (importing) View.VISIBLE else View.GONE
        importButton.isEnabled = !importing
        if (importing) {
            val total = progress.optLong("totalBytes")
            val done = progress.optLong("doneBytes")
            importBar.progress = if (total > 0) (1000 * done / total).toInt() else 0
            importLabel.text = "${progress.optString("name")}: ${progress.optString("file")}, ${done / MIB} of ${total / MIB} MiB"
        }
        if (importWasActive && !importing) {
            VmService.lastImportResult?.let {
                Toast.makeText(this, it.ifEmpty { "Import finished" }, Toast.LENGTH_LONG).show()
            }
            showVms()
        }
        importWasActive = importing

        val last = Native.lastRun()
        val run = if (last.isEmpty()) JSONObject() else JSONObject(last)
        val showRun = !vmUp && (run.optString("phase") in setOf("failed", "died") || (run.optString("phase") == "exited" && run.optInt("code", 0) != 0))
        banner.visibility = if (showRun) View.VISIBLE else View.GONE
        if (showRun) {
            lastRunLog = run.optString("logPath")
            banner.text = "${run.optString("vm")}: ${run.optString("message")}" + if (lastRunLog.isNotEmpty()) "\nTap for the log." else ""
        }
    }

    private fun showVms() {
        list.removeAllViews()
        val vms = JSONArray(Native.listVms())
        if (vms.length() == 0) {
            list.addView(TextView(this).apply {
                setPadding(0, 32, 0, 0)
                text = "No VMs yet. Create one, import a VM folder, or push one with adb to:\n${getExternalFilesDir(null)}/vms/<name>/"
            })
        }
        for (i in 0 until vms.length()) list.addView(row(vms.getJSONObject(i)))
    }

    private fun row(vm: JSONObject): View {
        val name = vm.getString("name")
        val problems = vm.getJSONArray("problems")
        val config = vm.getJSONObject("config")
        val detail = TextView(this).apply {
            text = if (problems.length() > 0) {
                (0 until problems.length()).joinToString("\n") { problems.getString(it) }
            } else {
                "${vm.getInt("width")}x${vm.getInt("height")}, ${config.getInt("vcpus")} vCPUs, " +
                    "${config.getInt("ramMib")} MiB RAM, disk ${vm.getLong("diskBytes") / (1024 * MIB)} GiB"
            }
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, 32, 0, 0)
            addView(TextView(this@MainActivity).apply {
                text = name
                textSize = 20f
            })
            addView(detail)
            addView(LinearLayout(this@MainActivity).apply {
                addView(Button(this@MainActivity).apply {
                    text = "Boot"
                    isAllCaps = false
                    isEnabled = problems.length() == 0
                    setOnClickListener { tryBoot(name) }
                })
                addView(Button(this@MainActivity).apply {
                    text = "Settings"
                    isAllCaps = false
                    setOnClickListener { settings(name, config) }
                })
            })
        }
    }

    private fun settings(name: String, config: JSONObject) {
        fun number(label: String, value: Int) = EditText(this).apply {
            hint = label
            inputType = InputType.TYPE_CLASS_NUMBER
            setText(value.toString())
        }
        fun labelled(parent: LinearLayout, label: String, field: View) {
            parent.addView(TextView(this).apply { text = label })
            parent.addView(field)
        }
        val vcpus = number("vCPUs", config.getInt("vcpus"))
        val ram = number("RAM (MiB)", config.getInt("ramMib"))
        val tb = number("Code cache (MiB)", config.getInt("tbMib"))
        val hints = CheckBox(this).apply {
            text = "Performance hints"
            isChecked = config.getBoolean("performanceHints")
        }
        val form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 24, 48, 0)
        }
        labelled(form, "vCPUs", vcpus)
        labelled(form, "RAM (MiB)", ram)
        labelled(form, "Code cache (MiB)", tb)
        form.addView(hints)
        AlertDialog.Builder(this)
            .setTitle(name)
            .setView(form)
            .setPositiveButton("Save") { _, _ ->
                val saved = JSONObject()
                    .put("vcpus", vcpus.text.toString().toIntOrNull() ?: 0)
                    .put("ramMib", ram.text.toString().toIntOrNull() ?: 0)
                    .put("tbMib", tb.text.toString().toIntOrNull() ?: 0)
                    .put("performanceHints", hints.isChecked)
                val error = Native.saveVmConfig(name, saved.toString())
                if (error.isNotEmpty()) Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                showVms()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun tryBoot(name: String) {
        val creating = Native.createPending()
        if (creating.isNotEmpty() && creating != name) {
            AlertDialog.Builder(this)
                .setTitle("A VM is being created")
                .setMessage("$creating is still being made, and only one VM can run at a time. Finish or delete it first.")
                .setPositiveButton("OK", null)
                .show()
            return
        }
        if (App.state().optBoolean("needsRestart")) {
            App.restart(this)
            return
        }
        val report = JSONObject(Native.preflight(name))
        val errors = report.getJSONArray("errors")
        if (errors.length() > 0) {
            AlertDialog.Builder(this)
                .setTitle("$name cannot start")
                .setMessage((0 until errors.length()).joinToString("\n\n") { errors.getString(it) })
                .setPositiveButton("OK", null)
                .show()
            return
        }
        val warnings = report.getJSONArray("warnings")
        if (warnings.length() > 0) {
            AlertDialog.Builder(this)
                .setTitle("Start $name?")
                .setMessage((0 until warnings.length()).joinToString("\n\n") { warnings.getString(it) })
                .setPositiveButton("Start") { _, _ -> askBatteryThenBoot(name) }
                .setNegativeButton("Cancel", null)
                .show()
        } else {
            askBatteryThenBoot(name)
        }
    }

    /**
     * A foreground service survives Doze, but Samsung's and OnePlus's battery managers still stop
     * apps under "optimized", which would end the VM. Asked once.
     */
    private fun askBatteryThenBoot(name: String) {
        val prefs = getSharedPreferences("ui", MODE_PRIVATE)
        val power = getSystemService(PowerManager::class.java)
        if (power.isIgnoringBatteryOptimizations(packageName) || prefs.getBoolean("askedBattery", false)) {
            boot(name)
            return
        }
        prefs.edit().putBoolean("askedBattery", true).apply()
        AlertDialog.Builder(this)
            .setTitle("Keep the VM running in the background?")
            .setMessage("This phone's battery manager can stop the VM when the app is not on screen. Allow vphone to run unrestricted to prevent that.")
            .setPositiveButton("Allow") { _, _ ->
                startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
                boot(name)
            }
            .setNegativeButton("Not now") { _, _ -> boot(name) }
            .show()
    }

    private fun boot(name: String) {
        VmService.boot(this, name)
        startActivity(Intent(this, VmActivity::class.java))
    }

    private fun pickFolder() {
        startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), PICK_FOLDER)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val tree = data?.data
        if (requestCode != PICK_FOLDER || resultCode != RESULT_OK || tree == null) return
        contentResolver.takePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION)
        VmService.import(this, tree)
    }

    companion object {
        const val EXTRA_BOOT = "boot"
        const val EXTRA_IMPORT = "import"
        private const val PICK_FOLDER = 2
        private const val MIB = 1024L * 1024
    }
}
