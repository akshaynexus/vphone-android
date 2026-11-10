package com.vphone.android

import android.Manifest
import android.app.Activity
import android.app.ActivityManager
import android.app.AlertDialog
import android.app.PendingIntent
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
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import org.json.JSONObject

/** The VMs in app-specific storage, importing one, and what happened to the last run. */
class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var banner: TextView
    private lateinit var active: Button
    private lateinit var activeLog: Button
    private lateinit var importButton: Button
    private lateinit var resumeCreate: Button
    private lateinit var importRow: LinearLayout
    private lateinit var importBar: ProgressBar
    private lateinit var importLabel: TextView
    private lateinit var list: LinearLayout
    private var importWasActive = false
    private lateinit var transferRow: LinearLayout
    private lateinit var transferLabel: TextView
    private lateinit var transferBar: ProgressBar
    private var transferWasActive = false
    private var transferSample: Pair<Long, Long>? = null
    /** The VM a "Save to…" is choosing a place for. */
    private var savingVm = ""
    private var lastRunLog = ""
    private var storageGeneration = 0L
    private lateinit var updateRow: LinearLayout
    private lateinit var updateText: TextView
    private lateinit var updateBar: ProgressBar
    private lateinit var updatePrimary: Button
    private lateinit var updateSecondary: Button
    private var shownUpdate: Updater.State? = null

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
            setOnClickListener { if (lastRunLog.isNotEmpty()) VmLogActivity.open(this@MainActivity, lastRunLog) }
        }
        active = Button(this).apply {
            isAllCaps = false
            visibility = View.GONE
            setOnClickListener { startActivity(Intent(this@MainActivity, VmActivity::class.java)) }
        }
        activeLog = Button(this).apply {
            text = "Its log"
            isAllCaps = false
            visibility = View.GONE
            setOnClickListener { VmLogActivity.open(this@MainActivity) }
        }
        importButton = Button(this).apply {
            text = "Import a VM"
            isAllCaps = false
            setOnClickListener { askImport() }
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
        transferLabel = TextView(this)
        transferBar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { max = 1000 }
        transferRow = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            addView(transferLabel)
            addView(transferBar)
            addView(Button(this@MainActivity).apply {
                text = "Cancel"
                isAllCaps = false
                setOnClickListener { Native.shareCancel() }
            })
        }
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        updateText = TextView(this)
        updateBar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { max = 1000 }
        updatePrimary = Button(this).apply { isAllCaps = false }
        updateSecondary = Button(this).apply { isAllCaps = false }
        updateRow = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            setPadding(0, 16, 0, 16)
            addView(updateText)
            addView(updateBar)
            addView(LinearLayout(this@MainActivity).apply {
                addView(updatePrimary)
                addView(updateSecondary)
            })
        }
        val info = JSONObject(Native.deviceInfo())
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(TextView(this@MainActivity).apply {
                text = "vphone ${BuildConfig.VERSION_NAME}"
                textSize = 28f
            })
            addView(TextView(this@MainActivity).apply {
                text = "${info.getInt("cpus")} CPU cores, ${info.getLong("totalMib") / 1024} GiB RAM, ${info.getLong("availableMib") / 1024} GiB free"
            })
            addView(banner)
            addView(updateRow)
            addView(active)
            addView(activeLog)
            addView(resumeCreate)
            addView(createButton)
            addView(importButton)
            addView(Button(this@MainActivity).apply {
                text = "Storage"
                isAllCaps = false
                setOnClickListener { startActivity(Intent(this@MainActivity, StorageActivity::class.java)) }
            })
            addView(Button(this@MainActivity).apply {
                text = "Check for updates"
                isAllCaps = false
                setOnClickListener { Updater.check(this@MainActivity, manual = true) }
            })
            addView(importRow)
            addView(transferRow)
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
        registerReceiver(usbPermission, android.content.IntentFilter(UsbVmSource.ACTION_PERMISSION), RECEIVER_NOT_EXPORTED)
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
        // A .vphone file opened from Quick Share, a file manager, or shared to this app.
        val received = when (intent.action) {
            Intent.ACTION_VIEW -> intent.data
            Intent.ACTION_SEND -> intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            else -> null
        }
        if (received != null) {
            intent.action = null
            confirmReceive(received)
        }
    }

    private fun askImport() {
        AlertDialog.Builder(this)
            .setTitle("Import a VM")
            .setItems(arrayOf("A .vphone file (sent from another phone)", "From a phone plugged in by USB", "A VM folder")) { _, which ->
                when (which) {
                    0 -> startActivityForResult(
                        Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*"),
                        PICK_VM_FILE,
                    )
                    1 -> usbImport()
                    else -> pickFolder()
                }
            }
            .show()
    }

    private val usbPermission = object : android.content.BroadcastReceiver() {
        override fun onReceive(context: android.content.Context, intent: Intent) {
            val device = intent.getParcelableExtra(android.hardware.usb.UsbManager.EXTRA_DEVICE, android.hardware.usb.UsbDevice::class.java) ?: return
            if (intent.getBooleanExtra(android.hardware.usb.UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                listUsbFiles(device)
            } else {
                Toast.makeText(this@MainActivity, "vphone needs USB access to read the other phone", Toast.LENGTH_LONG).show()
            }
        }
    }

    /** A VM from another phone on the cable: that phone in File transfer mode, this one in control. */
    private fun usbImport() {
        val devices = UsbVmSource.devices(this)
        if (devices.isEmpty()) {
            AlertDialog.Builder(this)
                .setTitle("No phone found on USB")
                .setMessage(
                    "1. On the sending phone, open the VM list, tap Send, then Save to… and choose Downloads.\n" +
                        "2. Connect the two phones with a USB-C cable.\n" +
                        "3. On the sending phone, tap the USB notification and choose File transfer. Keep it unlocked.\n" +
                        "4. On this phone, if Android asks, choose USB controlled by: This device.\n" +
                        "5. Tap Import a VM → From a phone plugged in by USB again.",
                )
                .setPositiveButton("OK", null)
                .show()
            return
        }
        if (devices.size == 1) {
            usbAccess(devices[0])
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Which device?")
            .setItems(devices.map { UsbVmSource.label(it) }.toTypedArray()) { _, i -> usbAccess(devices[i]) }
            .show()
    }

    private fun usbAccess(device: android.hardware.usb.UsbDevice) {
        val usb = getSystemService(android.hardware.usb.UsbManager::class.java)
        if (usb.hasPermission(device)) {
            listUsbFiles(device)
            return
        }
        val pending = PendingIntent.getBroadcast(
            this, 5, Intent(UsbVmSource.ACTION_PERMISSION).setPackage(packageName), PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        usb.requestPermission(device, pending)
    }

    private fun listUsbFiles(device: android.hardware.usb.UsbDevice) {
        Toast.makeText(this, "Looking for .vphone files on ${UsbVmSource.label(device)}", Toast.LENGTH_SHORT).show()
        Thread {
            val result = runCatching {
                val mtp = UsbVmSource.open(this, device)
                try { UsbVmSource.find(mtp, device.deviceName) } finally { mtp.close() }
            }
            handler.post {
                result.exceptionOrNull()?.let {
                    AlertDialog.Builder(this).setTitle("Cannot read ${UsbVmSource.label(device)}").setMessage(it.message ?: it.toString())
                        .setPositiveButton("OK", null).setNeutralButton("Copy") { _, _ -> App.copy(this, it.message ?: it.toString()) }.show()
                    return@post
                }
                val files = result.getOrThrow()
                if (files.isEmpty()) {
                    AlertDialog.Builder(this).setTitle("No VM files on ${UsbVmSource.label(device)}")
                        .setMessage("No .vphone file was found in its shared folders. On that phone, use Send → Save to… → Downloads, then try again.")
                        .setPositiveButton("OK", null).show()
                    return@post
                }
                AlertDialog.Builder(this)
                    .setTitle("Import from ${UsbVmSource.label(device)}")
                    .setItems(files.map { "${it.name}  (${it.size / MIB} MB, ${it.folder})" }.toTypedArray()) { _, i ->
                        if (JSONObject(Native.shareProgress()).optBoolean("active")) {
                            Toast.makeText(this, "Another VM is being sent or received", Toast.LENGTH_LONG).show()
                        } else {
                            VmService.receiveVmFromUsb(this, files[i])
                        }
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            }
        }.start()
    }

    override fun onDestroy() {
        runCatching { unregisterReceiver(usbPermission) }
        super.onDestroy()
    }

    private fun confirmReceive(uri: Uri) {
        val name = runCatching {
            contentResolver.query(uri, arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { if (it.moveToFirst()) it.getString(0) else null }
        }.getOrNull() ?: uri.lastPathSegment ?: "the file"
        AlertDialog.Builder(this)
            .setTitle("Import $name?")
            .setMessage("It is added to your VMs. A VM with the same name is kept, and this one gets a new name.")
            .setPositiveButton("Import") { _, _ ->
                runCatching { contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
                VmService.receiveVm(this, uri)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Send `vm`: Quick Share and other apps, or a file in a place the person picks. */
    private fun askSend(vm: String) {
        val state = App.state()
        val running = state.optString("vm") == vm && state.getString("phase") in setOf("preparing", "booting", "unlocking", "running", "stopping")
        val reason = when {
            running -> "$vm is running. Shut it down first, so its disk is complete."
            Native.createPending() == vm -> "$vm is still being created. Finish creating it first."
            JSONObject(Native.shareProgress()).optBoolean("active") -> "Another VM is being sent or received. Wait for it to finish."
            else -> ""
        }
        if (reason.isNotEmpty()) {
            AlertDialog.Builder(this).setTitle("Cannot send $vm").setMessage(reason).setPositiveButton("OK", null).show()
            return
        }
        val estimate = JSONObject(Native.shareEstimate(vm))
        estimate.optString("error").takeIf { it.isNotEmpty() }?.let {
            AlertDialog.Builder(this).setTitle("Cannot send $vm").setMessage(it).setPositiveButton("OK", null).show()
            return
        }
        val dataMb = estimate.optLong("dataBytes") / MIB
        val freeMb = (VmService.shareDir(this).apply { mkdirs() }.usableSpace) / MIB
        AlertDialog.Builder(this)
            .setTitle("Send $vm")
            .setMessage(
                "It holds about $dataMb MB of data, compressed as it is packed into one .vphone file. " +
                    "Sharing packs it on this phone first ($freeMb MB free there); saving writes it straight to the place you pick.\n\n" +
                    "Logs and this phone's settings stay here; the other phone uses its own.",
            )
            .setPositiveButton("Share…") { _, _ -> VmService.sendVm(this, vm, null) }
            .setNeutralButton("Save to…") { _, _ ->
                savingVm = vm
                startActivityForResult(
                    Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                        .setType("application/octet-stream").putExtra(Intent.EXTRA_TITLE, "$vm.vphone"),
                    SAVE_VM_FILE,
                )
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** The send or receive row, and what to do when one ends. */
    private fun showTransfer() {
        val p = JSONObject(Native.shareProgress())
        val active = p.optBoolean("active")
        transferRow.visibility = if (active) View.VISIBLE else View.GONE
        if (active) {
            val done = p.optLong("doneBytes")
            val total = p.optLong("totalBytes")
            val now = System.currentTimeMillis()
            val rate = transferSample?.let { (t, d) -> if (now > t && done >= d) (done - d) * 1000 / (now - t) else 0 } ?: 0
            transferSample = now to done
            val left = if (rate > 0 && total > done) ", about ${(total - done) / rate / 60} min ${(total - done) / rate % 60} s left" else ""
            val verb = if (p.optString("op") == "import") "Receiving" else "Packing"
            transferBar.progress = if (total > 0) (1000 * done / total).toInt() else 0
            transferLabel.text = "$verb ${p.optString("name")} (${p.optString("file")}): ${done / MIB} of ${total / MIB} MB, " +
                "${rate / MIB} MB/s$left; file so far ${p.optLong("archiveBytes") / MIB} MB"
        } else {
            transferSample = null
        }
        if (transferWasActive && !active) {
            VmService.lastTransfer?.let { t ->
                VmService.lastTransfer = null
                when {
                    t.error.isNotEmpty() -> AlertDialog.Builder(this)
                        .setTitle(if (t.op == "import") "The VM did not import" else "${t.name} was not sent")
                        .setMessage(t.error)
                        .setPositiveButton("OK", null)
                        .setNeutralButton("Copy") { _, _ -> App.copy(this, t.error) }
                        .show()
                    t.op == "import" -> {
                        Toast.makeText(this, "${t.name} is ready to boot", Toast.LENGTH_LONG).show()
                        showVms()
                    }
                    t.path.isNotEmpty() -> VmService.shareIntent(this, t)?.let { startActivity(it) }
                    else -> Toast.makeText(this, "${t.name} is saved (${t.bytes / MIB} MB)", Toast.LENGTH_LONG).show()
                }
            }
        }
        transferWasActive = active
    }

    override fun onStart() {
        super.onStart()
        noteDeadProcess()
        showVms()
        Updater.checkIfDue(this)
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
        showUpdate()
        showTransfer()
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
        activeLog.visibility = active.visibility
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

    /** The update row: what was found, the download's progress, and the next step. */
    private fun showUpdate() {
        Updater.pendingConfirm?.let {
            Updater.pendingConfirm = null
            startActivity(it)
        }
        val s = Updater.state
        if (s === shownUpdate && s !is Updater.State.Downloading) return
        val first = shownUpdate
        shownUpdate = s
        fun buttons(primary: String?, onPrimary: () -> Unit, secondary: String? = null, onSecondary: () -> Unit = {}) {
            updatePrimary.visibility = if (primary == null) View.GONE else View.VISIBLE
            updatePrimary.text = primary
            updatePrimary.setOnClickListener { onPrimary() }
            updateSecondary.visibility = if (secondary == null) View.GONE else View.VISIBLE
            updateSecondary.text = secondary
            updateSecondary.setOnClickListener { onSecondary() }
        }
        updateRow.visibility = View.VISIBLE
        updateBar.visibility = View.GONE
        when (s) {
            is Updater.State.Idle -> updateRow.visibility = View.GONE
            is Updater.State.Checking -> {
                updateText.text = "Checking for updates"
                buttons(null, {})
            }
            is Updater.State.UpToDate -> {
                updateRow.visibility = View.GONE
                if (s.manual && first !is Updater.State.UpToDate) {
                    Toast.makeText(this, "vphone ${BuildConfig.VERSION_NAME} is the newest release", Toast.LENGTH_SHORT).show()
                }
            }
            is Updater.State.Available -> {
                updateText.text = "vphone ${s.release.version} is available (${s.release.size / MIB} MB)"
                buttons("Download and install", { Updater.download(this, s.release) }, "What's new", { notes(s.release) })
            }
            is Updater.State.Downloading -> {
                updateBar.visibility = View.VISIBLE
                updateBar.progress = if (s.total > 0) (1000 * s.done / s.total).toInt() else 0
                val left = if (s.bytesPerSec > 0 && s.total > s.done) ", ${(s.total - s.done) / s.bytesPerSec} s left" else ""
                updateText.text = "Downloading ${s.release.version}: ${s.done / MIB} of ${s.total / MIB} MB, " +
                    "${s.bytesPerSec / MIB} MB/s$left"
                if (first !is Updater.State.Downloading) buttons("Cancel", { Updater.cancel() })
            }
            is Updater.State.Ready -> {
                updateText.text = "vphone ${s.release.version} is downloaded"
                buttons("Install", { askInstall(s.release, s.apk) }, "What's new", { notes(s.release) })
            }
            is Updater.State.Installing -> {
                updateText.text = "Installing ${s.release.version}: the app restarts when it is done"
                buttons(null, {})
            }
            is Updater.State.Failed -> {
                updateText.text = s.message
                updateText.setTextIsSelectable(true)
                val r = s.release
                if (r != null) {
                    buttons("Download", { Updater.download(this, r) }, "Copy error", { App.copy(this, s.message) })
                } else {
                    buttons("Try again", { Updater.check(this, manual = true) }, "Copy error", { App.copy(this, s.message) })
                }
            }
        }
    }

    private fun notes(release: Updater.Release) {
        AlertDialog.Builder(this)
            .setTitle("vphone ${release.version}")
            .setMessage(release.notes.ifEmpty { "No notes." })
            .setPositiveButton("Close", null)
            .setNeutralButton("Open on GitHub") { _, _ -> startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(release.page))) }
            .show()
    }

    /** Installing replaces the app, which ends whatever its processes are running. */
    private fun askInstall(release: Updater.Release, apk: java.io.File) {
        if (!Updater.canInstall(this)) {
            AlertDialog.Builder(this)
                .setTitle("Allow vphone to install updates")
                .setMessage("Android asks once: turn on \"Allow from this source\" for vphone, come back, and tap Install again.")
                .setPositiveButton("Open the setting") { _, _ ->
                    startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:$packageName")))
                }
                .setNegativeButton("Cancel", null)
                .show()
            return
        }
        val phase = App.state().getString("phase")
        val vmUp = phase in setOf("preparing", "booting", "unlocking", "running", "stopping")
        val creating = Native.createPending()
        val warning = when {
            vmUp -> "${App.state().optString("vm")} is running. Installing ends it at once, without iOS's shutdown."
            creating.isNotEmpty() -> "$creating is being created. Installing stops the step it is on; it continues from there when you open the app again."
            else -> ""
        }
        if (warning.isEmpty()) {
            Updater.install(this, release, apk)
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Install vphone ${release.version}?")
            .setMessage(warning)
            .setPositiveButton("Install") { _, _ -> Updater.install(this, release, apk) }
            .setNegativeButton("Later", null)
            .show()
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
                    text = "Send"
                    isAllCaps = false
                    isEnabled = problems.length() == 0
                    setOnClickListener { askSend(name) }
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
            text = "Performance hints (tell Android how busy the emulated CPUs are)"
            isChecked = config.getBoolean("performanceHints")
        }
        val performanceValues = listOf("max", "balanced")
        val performance = Spinner(this).apply {
            adapter = ArrayAdapter(
                this@MainActivity,
                android.R.layout.simple_spinner_dropdown_item,
                listOf("Max (highest CPU clocks; warmer, more battery)", "Balanced (cooler, slower)"),
            )
            setSelection(performanceValues.indexOf(config.optString("performance", "max")).coerceAtLeast(0))
        }
        val refreshValues = listOf(120, 60)
        val refresh = Spinner(this).apply {
            adapter = ArrayAdapter(
                this@MainActivity,
                android.R.layout.simple_spinner_dropdown_item,
                listOf("120 Hz", "60 Hz"),
            )
            setSelection(refreshValues.indexOf(config.optInt("refreshHz", 120)).coerceAtLeast(0))
        }
        // crate::services::Level, in the order of the labels.
        val servicesValues = listOf("unchanged", "trimmed", "fewest")
        val services = Spinner(this).apply {
            adapter = ArrayAdapter(
                this@MainActivity,
                android.R.layout.simple_spinner_dropdown_item,
                listOf(
                    "As they are",
                    "Trimmed (about 138 off: Siri, iCloud sync, analytics, Journal...)",
                    "Fewest (experimental: also no widgets, no AirDrop)",
                ),
            )
            setSelection(servicesValues.indexOf(config.optString("services", "unchanged")).coerceAtLeast(0))
        }
        val network = CheckBox(this).apply {
            text = "Network (internet for iOS through this phone's connection; IPv4 only)"
            isChecked = config.optBoolean("network", true)
        }
        val sound = CheckBox(this).apply {
            text = "Sound (iOS plays through this phone's speaker or headphones; no microphone)"
            isChecked = config.optBoolean("sound", true)
        }
        val display = CheckBox(this).apply {
            text = "Show the screen (off: iOS runs with nothing drawn)"
            isChecked = config.optBoolean("display", true)
        }
        val compression = CheckBox(this).apply {
            text = "GPU image compression (UBWC) for the driver's own images. Faster where it works; " +
                "turn it off if the picture is wrong"
            isChecked = config.optBoolean("gpuCompression", false)
        }
        // The values the driver reads (TU_ANDROID_SWAPCHAIN_LAYOUT), in the order of the labels.
        val layoutValues = listOf("auto", "linear", "ubwc")
        val swapchainLayout = Spinner(this).apply {
            adapter = ArrayAdapter(
                this@MainActivity,
                android.R.layout.simple_spinner_dropdown_item,
                listOf("Auto (as gralloc says, else linear)", "Linear", "Compressed (UBWC)"),
            )
            setSelection(layoutValues.indexOf(config.optString("swapchainLayout", "auto")).coerceAtLeast(0))
        }
        val softwareDisplay = CheckBox(this).apply {
            text = "Draw the screen on the CPU (slower; right whatever the GPU driver does with the " +
                "screen's buffers)"
            isChecked = config.optBoolean("softwareDisplay", false)
        }
        val diagnostics = CheckBox(this).apply {
            text = "GPU diagnostics: image layouts in the log, and pictures of the guest's screen " +
                "in the log screen's Share"
            isChecked = config.optBoolean("gpuDiagnostics", false)
        }
        val form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 24, 48, 0)
        }
        labelled(form, "vCPUs", vcpus)
        labelled(form, "RAM (MiB)", ram)
        labelled(form, "Code cache (MiB)", tb)
        labelled(form, "Performance", performance)
        form.addView(hints)
        form.addView(network)
        form.addView(sound)
        labelled(
            form,
            "iOS background services. Fewer leave more of the emulated CPUs to the screen. A change is " +
                "applied at the VM's next start and takes effect at the start after that",
            services,
        )
        labelled(form, "Refresh rate (iOS's display mode, and the rate asked of this phone's screen)", refresh)
        form.addView(display)
        form.addView(compression)
        labelled(
            form,
            "Screen buffer layout. If the picture is smeared into bands or dashes, try the other two",
            swapchainLayout,
        )
        form.addView(softwareDisplay)
        form.addView(diagnostics)
        AlertDialog.Builder(this)
            .setTitle(name)
            .setView(ScrollView(this).apply { addView(form) })
            .setPositiveButton("Save") { _, _ ->
                val saved = JSONObject()
                    .put("vcpus", vcpus.text.toString().toIntOrNull() ?: 0)
                    .put("ramMib", ram.text.toString().toIntOrNull() ?: 0)
                    .put("tbMib", tb.text.toString().toIntOrNull() ?: 0)
                    .put("version", config.optInt("version", 2))
                    .put("performance", performanceValues[performance.selectedItemPosition.coerceIn(0, 1)])
                    .put("refreshHz", refreshValues[refresh.selectedItemPosition.coerceIn(0, 1)])
                    .put("performanceHints", hints.isChecked)
                    .put("services", servicesValues[services.selectedItemPosition.coerceIn(0, 2)])
                    .put("network", network.isChecked)
                    .put("sound", sound.isChecked)
                    .put("display", display.isChecked)
                    .put("gpuCompression", compression.isChecked)
                    .put("gpuDiagnostics", diagnostics.isChecked)
                    .put("softwareDisplay", softwareDisplay.isChecked)
                    .put("swapchainLayout", layoutValues[swapchainLayout.selectedItemPosition.coerceIn(0, 2)])
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
        val uri = data?.data
        if (resultCode != RESULT_OK || uri == null) return
        when (requestCode) {
            PICK_FOLDER -> {
                contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
                VmService.import(this, uri)
            }
            PICK_VM_FILE -> confirmReceive(uri)
            SAVE_VM_FILE -> if (savingVm.isNotEmpty()) VmService.sendVm(this, savingVm, uri)
        }
    }

    companion object {
        const val EXTRA_BOOT = "boot"
        const val EXTRA_IMPORT = "import"
        private const val PICK_FOLDER = 2
        private const val PICK_VM_FILE = 3
        private const val SAVE_VM_FILE = 4
        private const val MIB = 1024L * 1024
    }
}
