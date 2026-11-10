package com.vphone.android

import android.Manifest
import android.app.ActivityManager
import android.app.ApplicationExitInfo
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.widget.Toast
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
import com.vphone.android.ui.FailureUi
import com.vphone.android.ui.MainActions
import com.vphone.android.ui.MainScreen
import com.vphone.android.ui.MainUi
import com.vphone.android.ui.ProgressUi
import com.vphone.android.ui.UpdateUi
import com.vphone.android.ui.VmCardUi
import com.vphone.android.ui.VmStatus
import com.vphone.android.ui.VphoneTheme
import com.vphone.android.ui.leftText
import com.vphone.android.ui.rateText
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/** The VMs in app-specific storage, importing one, and what happened to the last run. */
class MainActivity : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()

    /** What the screen shows: built by [refresh] every second, drawn by [MainScreen]. */
    private var ui by mutableStateOf(MainUi(BuildConfig.VERSION_NAME))
    private val dialogs = DialogHost()

    /** The VMs as the native side listed them, before what each is doing is added. */
    private var listed: List<VmCardUi> = emptyList()
    private var device = ""
    private var importWasActive = false
    private var importSample: Pair<Long, Long>? = null
    private var importRate = 0L
    private var transferWasActive = false
    private var transferSample: Pair<Long, Long>? = null
    /** The VM a "Save to…" is choosing a place for. */
    private var savingVm = ""
    private var lastRunLog = ""
    private var storageGeneration = 0L
    private var shownUpdate: Updater.State? = null
    /** The creation under way, as its status file had it at the last poll. */
    @Volatile private var creation: VmStatus.Creating? = null
    private var creationFor = ""
    private val creationPolling = AtomicBoolean(false)

    private val poll = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val actions = MainActions(
            boot = ::tryBoot,
            open = { startActivity(Intent(this, VmActivity::class.java)) },
            continueCreate = { startActivity(Intent(this, CreateActivity::class.java)) },
            settings = { name -> startActivity(Intent(this, VmSettingsActivity::class.java).putExtra(VmSettingsActivity.EXTRA_VM, name)) },
            send = ::askSend,
            log = ::openLog,
            delete = { name -> startActivity(Intent(this, StorageActivity::class.java).putExtra(StorageActivity.EXTRA_DELETE, name)) },
            create = { startActivity(Intent(this, CreateActivity::class.java)) },
            importFile = ::pickVmFile,
            importUsb = ::usbImport,
            importFolder = ::pickFolder,
            storage = { startActivity(Intent(this, StorageActivity::class.java)) },
            checkUpdates = { Updater.check(this, manual = true) },
            updatePrimary = ::updatePrimary,
            updateNotes = {
                when (val s = Updater.state) {
                    is Updater.State.Available -> notes(s.release)
                    is Updater.State.Ready -> notes(s.release)
                    else -> Unit
                }
            },
            updateCancel = { Updater.cancel() },
            cancelImport = { VmService.cancelImport() },
            cancelTransfer = { Native.shareCancel() },
            failureLog = { if (lastRunLog.isNotEmpty()) VmLogActivity.open(this, lastRunLog) },
            copy = { App.copy(this, it) },
        )
        setContent {
            VphoneTheme {
                MainScreen(ui, actions)
                dialogs.current?.let { spec ->
                    AppDialog(spec, onCopy = { App.copy(this, it) }, onDismiss = { dialogs.current = null })
                }
            }
        }
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
        registerReceiver(usbPermission, android.content.IntentFilter(UsbVmSource.ACTION_PERMISSION), RECEIVER_NOT_EXPORTED)
        handleIntent(intent)
    }

    private fun ask(title: String, message: String) = dialogs.show(DialogSpec(title, message))

    private fun fail(title: String, message: String) = dialogs.show(DialogSpec.error(title, message))

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

    private fun pickVmFile() {
        startActivityForResult(
            Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*"),
            PICK_VM_FILE,
        )
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
            ask(
                "No phone found on USB",
                "1. On the sending phone, open the VM list, tap Send, then Save to… and choose Downloads.\n" +
                    "2. Connect the two phones with a USB-C cable.\n" +
                    "3. On the sending phone, tap the USB notification and choose File transfer. Keep it unlocked.\n" +
                    "4. On this phone, if Android asks, choose USB controlled by: This device.\n" +
                    "5. Tap + → Import from a phone by USB again.",
            )
            return
        }
        if (devices.size == 1) {
            usbAccess(devices[0])
            return
        }
        dialogs.show(
            DialogSpec(
                "Which device?",
                items = devices.map { d -> DialogAction(UsbVmSource.label(d)) { usbAccess(d) } },
                confirm = null,
                dismiss = DialogAction("Cancel"),
            ),
        )
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
                    fail("Cannot read ${UsbVmSource.label(device)}", it.message ?: it.toString())
                    return@post
                }
                val files = result.getOrThrow()
                if (files.isEmpty()) {
                    ask(
                        "No VM files on ${UsbVmSource.label(device)}",
                        "No .vphone file was found in its shared folders. On that phone, use Send → Save to… → Downloads, then try again.",
                    )
                    return@post
                }
                dialogs.show(
                    DialogSpec(
                        "Import from ${UsbVmSource.label(device)}",
                        items = files.map { f ->
                            DialogAction("${f.name}  (${f.size / MIB} MB, ${f.folder})") {
                                if (JSONObject(Native.shareProgress()).optBoolean("active")) {
                                    Toast.makeText(this, "Another VM is being sent or received", Toast.LENGTH_LONG).show()
                                } else {
                                    VmService.receiveVmFromUsb(this, f)
                                }
                            }
                        },
                        confirm = null,
                        dismiss = DialogAction("Cancel"),
                    ),
                )
            }
        }.start()
    }

    override fun onDestroy() {
        worker.shutdown()
        runCatching { unregisterReceiver(usbPermission) }
        super.onDestroy()
    }

    private fun confirmReceive(uri: Uri) {
        val name = runCatching {
            contentResolver.query(uri, arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { if (it.moveToFirst()) it.getString(0) else null }
        }.getOrNull() ?: uri.lastPathSegment ?: "the file"
        dialogs.show(
            DialogSpec(
                "Import $name?",
                "It is added to your VMs. A VM with the same name is kept, and this one gets a new name.",
                confirm = DialogAction("Import") {
                    runCatching { contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
                    VmService.receiveVm(this, uri)
                },
                dismiss = DialogAction("Cancel"),
            ),
        )
    }

    /** Send `vm`: Quick Share and other apps, or a file in a place the person picks. */
    private fun askSend(vm: String) {
        val state = App.state()
        val running = state.optString("vm") == vm && state.getString("phase") in UP
        val reason = when {
            running -> "$vm is running. Shut it down first, so its disk is complete."
            Native.createPending() == vm -> "$vm is still being created. Finish creating it first."
            JSONObject(Native.shareProgress()).optBoolean("active") -> "Another VM is being sent or received. Wait for it to finish."
            else -> ""
        }
        if (reason.isNotEmpty()) {
            ask("Cannot send $vm", reason)
            return
        }
        val estimate = JSONObject(Native.shareEstimate(vm))
        estimate.optString("error").takeIf { it.isNotEmpty() }?.let {
            fail("Cannot send $vm", it)
            return
        }
        val dataMb = estimate.optLong("dataBytes") / MIB
        val freeMb = (VmService.shareDir(this).apply { mkdirs() }.usableSpace) / MIB
        dialogs.show(
            DialogSpec(
                "Send $vm",
                "It holds about $dataMb MB of data, compressed as it is packed into one .vphone file. " +
                    "Sharing packs it on this phone first ($freeMb MB free there); saving writes it straight to the place you pick.\n\n" +
                    "Logs and this phone's settings stay here; the other phone uses its own.",
                confirm = DialogAction("Share…") { VmService.sendVm(this, vm, null) },
                neutral = DialogAction("Save to…") {
                    savingVm = vm
                    startActivityForResult(
                        Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                            .setType("application/octet-stream").putExtra(Intent.EXTRA_TITLE, "$vm.vphone"),
                        SAVE_VM_FILE,
                    )
                },
                dismiss = DialogAction("Cancel"),
            ),
        )
    }

    /** The send or receive card, and what to do when one ends. `p` is `Native.shareProgress()`. */
    private fun transferUi(p: JSONObject): ProgressUi? {
        val active = p.optBoolean("active")
        var card: ProgressUi? = null
        if (active) {
            val done = p.optLong("doneBytes")
            val total = p.optLong("totalBytes")
            val now = System.currentTimeMillis()
            val rate = transferSample?.let { (t, d) -> if (now > t && done >= d) (done - d) * 1000 / (now - t) else 0 } ?: 0
            transferSample = now to done
            val receiving = p.optString("op") == "import"
            card = ProgressUi(
                title = "${if (receiving) "Receiving" else "Packing"} ${p.optString("name")}",
                detail = p.optString("file"),
                fraction = if (total > 0) done.toFloat() / total else null,
                stats = listOf(
                    "${done / MIB} of ${total / MIB} MB",
                    rateText(rate),
                    if (rate > 0 && total > done) leftText((total - done) / rate) else "",
                    "file so far ${p.optLong("archiveBytes") / MIB} MB",
                ),
                cancelLabel = "Cancel",
            )
        } else {
            transferSample = null
        }
        if (transferWasActive && !active) {
            VmService.lastTransfer?.let { t ->
                VmService.lastTransfer = null
                when {
                    t.error.isNotEmpty() -> fail(if (t.op == "import") "The VM did not import" else "${t.name} was not sent", t.error)
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
        return card
    }

    override fun onStart() {
        super.onStart()
        noteDeadProcess()
        val info = JSONObject(Native.deviceInfo())
        device = "${info.getInt("cpus")} CPU cores, ${info.getLong("totalMib") / 1024} GiB RAM, ${info.getLong("availableMib") / 1024} GiB free"
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
        val update = updateUi()
        val share = JSONObject(Native.shareProgress())
        val transfer = transferUi(share)
        val cleaned = StorageNative.generation()
        if (cleaned != storageGeneration) {
            storageGeneration = cleaned
            showVms()
        }
        val creating = Native.createPending()
        pollCreation(creating)
        val state = App.state()
        val phase = state.getString("phase")
        val vmUp = phase in UP
        val runningVm = if (vmUp) state.optString("vm") else ""

        val progress = JSONObject(Native.importProgress())
        val importing = progress.optBoolean("active")
        var folderImport: ProgressUi? = null
        if (importing) {
            val total = progress.optLong("totalBytes")
            val done = progress.optLong("doneBytes")
            val now = System.currentTimeMillis()
            importSample?.let { (t, d) -> if (now > t && done >= d) importRate = (done - d) * 1000 / (now - t) }
            importSample = now to done
            folderImport = ProgressUi(
                title = "Importing ${progress.optString("name")}",
                detail = progress.optString("file"),
                fraction = if (total > 0) done.toFloat() / total else null,
                stats = listOf(
                    "${done / MIB} of ${total / MIB} MiB",
                    rateText(importRate),
                    if (importRate > 0 && total > done) leftText((total - done) / importRate) else "",
                ),
                cancelLabel = "Cancel import",
            )
        } else {
            importSample = null
            importRate = 0
        }
        if (importWasActive && !importing) {
            VmService.lastImportResult?.let {
                if (it.isEmpty()) {
                    Toast.makeText(this, "Import finished", Toast.LENGTH_LONG).show()
                } else {
                    fail("The VM did not import", it)
                }
            }
            showVms()
        }
        importWasActive = importing

        val last = Native.lastRun()
        val run = if (last.isEmpty()) JSONObject() else JSONObject(last)
        val showRun = !vmUp && (run.optString("phase") in setOf("failed", "died") || (run.optString("phase") == "exited" && run.optInt("code", 0) != 0))
        var failure: FailureUi? = null
        if (showRun) {
            lastRunLog = run.optString("logPath")
            failure = FailureUi(run.optString("vm"), run.optString("message"), lastRunLog.isNotEmpty())
        }

        // What each VM is doing; one that is up or being made but not listed still gets its card.
        val started = state.optLong("startedMs")
        val running = VmStatus.Running(
            VmService.phaseText(phase),
            if (started > 0 && phase in TIMED) VmLogActivity.clock((System.currentTimeMillis() - started) / 1000) else "",
            phase == "running",
        )
        val making = if (creating.isEmpty()) null else creation?.takeIf { creationFor == creating } ?: VmStatus.Creating("getting ready", null, false)
        val sendingVm = if (share.optBoolean("active") && share.optString("op") != "import") share.optString("name") else ""
        val sending = VmStatus.Sending(transfer?.fraction)
        var vms = listed.map { vm ->
            when (vm.name) {
                runningVm -> vm.copy(status = running)
                creating -> vm.copy(status = making!!)
                sendingVm -> vm.copy(status = sending)
                else -> vm
            }
        }
        if (creating.isNotEmpty() && vms.none { it.name == creating }) vms = listOf(VmCardUi(creating, status = making!!)) + vms
        if (runningVm.isNotEmpty() && vms.none { it.name == runningVm }) vms = listOf(VmCardUi(runningVm, status = running)) + vms
        vms = vms.sortedBy {
            when (it.status) {
                is VmStatus.Running -> 0
                is VmStatus.Creating -> 1
                else -> 2
            }
        }
        ui = MainUi(
            version = BuildConfig.VERSION_NAME,
            device = device,
            vms = vms,
            vmsPath = "${getExternalFilesDir(null)}/vms/<name>/",
            failure = failure,
            update = update,
            folderImport = folderImport,
            transfer = transfer,
        )
    }

    /** The creation's step and progress for its card: file work, so off the main thread. */
    private fun pollCreation(name: String) {
        if (name.isEmpty()) {
            creation = null
            creationFor = ""
            return
        }
        if (!creationPolling.compareAndSet(false, true)) return
        worker.execute {
            val s = runCatching { JSONObject(Native.createStatus(name)) }.getOrNull()
            creationPolling.set(false)
            if (s == null) return@execute
            val made = creationStatus(s)
            handler.post {
                creation = made
                creationFor = name
            }
        }
    }

    private fun creationStatus(s: JSONObject): VmStatus.Creating {
        val state = s.optString("state")
        val all = s.optJSONArray("steps")
        val steps = (0 until (all?.length() ?: 0)).map { all!!.getJSONObject(it) }
        val at = steps.indexOfFirst { it.optString("key") == s.optString("step") }
        val step = if (at >= 0) "step ${at + 1} of ${steps.size}, ${steps[at].optString("title")}" else "getting ready"
        val total = s.optLong("totalBytes")
        val left = s.optLong("etaSecs")
        val text = when (state) {
            "done" -> "all ${steps.size} steps finished"
            "failed" -> "$step failed"
            "cancelled", "interrupted" -> "$step, waiting to be resumed"
            else -> if (left > 0) "$step, about ${CreateActivity.durationText(left)} left" else step
        }
        return VmStatus.Creating(
            text = text,
            fraction = if (total > 0) s.optLong("doneBytes").toFloat() / total else null,
            paused = s.optBoolean("paused") || state in setOf("cancelled", "interrupted", "done"),
            failed = state == "failed",
        )
    }

    /** The update card: what was found, the download's progress, and the next step. */
    private fun updateUi(): UpdateUi {
        Updater.pendingConfirm?.let {
            Updater.pendingConfirm = null
            startActivity(it)
        }
        val s = Updater.state
        val first = shownUpdate
        shownUpdate = s
        return when (s) {
            is Updater.State.Idle -> UpdateUi.Hidden
            is Updater.State.Checking -> UpdateUi.Checking
            is Updater.State.UpToDate -> {
                if (s.manual && first !== s) {
                    Toast.makeText(this, "vphone ${BuildConfig.VERSION_NAME} is the newest release", Toast.LENGTH_SHORT).show()
                }
                UpdateUi.Hidden
            }
            is Updater.State.Available -> UpdateUi.Available(s.release.version, s.release.size / MIB)
            is Updater.State.Downloading -> UpdateUi.Downloading(s.release.version, s.done, s.total, s.bytesPerSec)
            is Updater.State.Ready -> UpdateUi.Ready(s.release.version)
            is Updater.State.Installing -> UpdateUi.Installing(s.release.version)
            is Updater.State.Failed -> UpdateUi.Failed(s.message, s.release != null)
        }
    }

    /** The update card's main button, for the state the updater is in. */
    private fun updatePrimary() {
        when (val s = Updater.state) {
            is Updater.State.Available -> Updater.download(this, s.release)
            is Updater.State.Ready -> askInstall(s.release, s.apk)
            is Updater.State.Failed -> {
                val r = s.release
                if (r != null) Updater.download(this, r) else Updater.check(this, manual = true)
            }
            else -> Unit
        }
    }

    private fun notes(release: Updater.Release) {
        dialogs.show(
            DialogSpec(
                "vphone ${release.version}",
                release.notes.ifEmpty { "No notes." },
                confirm = DialogAction("Close"),
                neutral = DialogAction("Open on GitHub") { startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(release.page))) },
            ),
        )
    }

    /** Installing replaces the app, which ends whatever its processes are running. */
    private fun askInstall(release: Updater.Release, apk: java.io.File) {
        if (!Updater.canInstall(this)) {
            dialogs.show(
                DialogSpec(
                    "Allow vphone to install updates",
                    "Android asks once: turn on \"Allow from this source\" for vphone, come back, and tap Install again.",
                    confirm = DialogAction("Open the setting") {
                        startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:$packageName")))
                    },
                    dismiss = DialogAction("Cancel"),
                ),
            )
            return
        }
        val phase = App.state().getString("phase")
        val vmUp = phase in UP
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
        dialogs.show(
            DialogSpec(
                "Install vphone ${release.version}?",
                warning,
                confirm = DialogAction("Install") { Updater.install(this, release, apk) },
                dismiss = DialogAction("Later"),
            ),
        )
    }

    /** Reads the VMs again; [refresh] adds what each is doing. */
    private fun showVms() {
        val vms = JSONArray(Native.listVms())
        listed = (0 until vms.length()).map { vms.getJSONObject(it) }.map { vm ->
            val problems = vm.getJSONArray("problems")
            val config = vm.getJSONObject("config")
            VmCardUi(
                name = vm.getString("name"),
                problems = (0 until problems.length()).map { problems.getString(it) },
                width = vm.optInt("width"),
                height = vm.optInt("height"),
                vcpus = config.getInt("vcpus"),
                ramMib = config.getInt("ramMib"),
                diskBytes = vm.optLong("diskBytes"),
            )
        }
        if (ui.vmsPath.isNotEmpty()) refresh()
    }

    /**
     * A VM's log: the live one while it runs, the wizard's while it is made, and otherwise the
     * newest one in its folder, which the storage scan finds.
     */
    private fun openLog(vm: VmCardUi) {
        when (vm.status) {
            is VmStatus.Running -> VmLogActivity.open(this)
            is VmStatus.Creating -> CreateLogActivity.open(this, vm.name)
            else -> worker.execute {
                val path = runCatching {
                    val all = JSONObject(StorageNative.scan()).getJSONArray("vms")
                    (0 until all.length()).map { all.getJSONObject(it) }
                        .firstOrNull { it.optString("name") == vm.name }?.optString("log").orEmpty()
                }.getOrDefault("")
                handler.post {
                    if (isDestroyed) return@post
                    if (path.isEmpty()) ask("No log yet", "${vm.name} has not written a log on this phone.") else VmLogActivity.open(this, path)
                }
            }
        }
    }

    private fun tryBoot(name: String) {
        val creating = Native.createPending()
        if (creating.isNotEmpty() && creating != name) {
            ask("A VM is being created", "$creating is still being made, and only one VM can run at a time. Finish or delete it first.")
            return
        }
        if (App.state().optBoolean("needsRestart")) {
            App.restart(this)
            return
        }
        val report = JSONObject(Native.preflight(name))
        val errors = report.getJSONArray("errors")
        if (errors.length() > 0) {
            fail("$name cannot start", (0 until errors.length()).joinToString("\n\n") { errors.getString(it) })
            return
        }
        val warnings = report.getJSONArray("warnings")
        if (warnings.length() > 0) {
            dialogs.show(
                DialogSpec(
                    "Start $name?",
                    (0 until warnings.length()).joinToString("\n\n") { warnings.getString(it) },
                    confirm = DialogAction("Start") { askBatteryThenBoot(name) },
                    dismiss = DialogAction("Cancel"),
                ),
            )
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
        dialogs.show(
            DialogSpec(
                "Keep the VM running in the background?",
                "This phone's battery manager can stop the VM when the app is not on screen. Allow vphone to run unrestricted to prevent that.",
                confirm = DialogAction("Allow") {
                    startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
                    boot(name)
                },
                dismiss = DialogAction("Not now") { boot(name) },
            ),
        )
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
        private val UP = setOf("preparing", "booting", "unlocking", "running", "stopping")
        /** The phases whose time since the start is shown. */
        private val TIMED = setOf("booting", "unlocking", "running", "stopping")
    }
}
