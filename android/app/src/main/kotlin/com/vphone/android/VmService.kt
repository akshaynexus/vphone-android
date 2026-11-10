package com.vphone.android

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.Uri
import android.os.IBinder
import android.os.ParcelFileDescriptor
import android.os.PowerManager
import android.provider.DocumentsContract
import org.json.JSONObject
import kotlin.concurrent.thread

/** Keeps the process, and the CPU, alive for a running VM or an import. */
class VmService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null
    @Volatile private var vmActive = false
    @Volatile private var importing = false
    @Volatile private var transferring = false
    private var monitor: Thread? = null
    private var lastText = ""

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_BOOT -> {
                vmActive = true
                goForeground("Starting")
                val error = Native.boot(intent.getStringExtra(EXTRA_VM) ?: "")
                if (error.isNotEmpty()) {
                    vmActive = false
                    lastBootError = error
                }
                startMonitor()
            }
            ACTION_STOP -> Native.stop()
            ACTION_IMPORT -> {
                importing = true
                goForeground("Importing a VM")
                val tree = intent.data
                if (tree != null) thread(name = "vm-import") { import(tree) } else importing = false
                startMonitor()
            }
            ACTION_CANCEL_IMPORT -> Native.importCancel()
            ACTION_SEND_VM, ACTION_RECEIVE_VM, ACTION_RECEIVE_USB -> {
                if (transferring) return START_NOT_STICKY
                transferring = true
                lastTransfer = null
                val sending = intent.action == ACTION_SEND_VM
                goForeground(if (sending) "Preparing a VM to send" else "Receiving a VM")
                val vm = intent.getStringExtra(EXTRA_VM).orEmpty()
                val uri = intent.data
                val usbDevice = intent.getStringExtra(EXTRA_USB_DEVICE)
                val usbHandle = intent.getIntExtra(EXTRA_USB_HANDLE, 0)
                thread(name = "vm-transfer") {
                    lastTransfer = when {
                        sending -> sendVm(vm, uri)
                        usbDevice != null -> receiveVmFromUsb(usbDevice, usbHandle)
                        else -> receiveVm(uri)
                    }
                    transferring = false
                }
                startMonitor()
            }
            ACTION_CANCEL_TRANSFER -> Native.shareCancel()
            else -> if (!vmActive && !importing && !transferring) stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun goForeground(text: String) {
        startForeground(NOTIFICATION, notification(text), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        if (wakeLock == null) {
            wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "vphone:vm")
                .apply { acquire() }
        }
    }

    private fun notification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, if (vmActive) VmActivity::class.java else MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val builder = Notification.Builder(this, App.CHANNEL)
            .setSmallIcon(R.drawable.ic_phone)
            .setContentTitle("vphone")
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
        if (vmActive) {
            val stop = PendingIntent.getService(
                this, 1, Intent(this, VmService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE,
            )
            builder.addAction(Notification.Action.Builder(null, "Stop VM", stop).build())
        }
        return builder.build()
    }

    private fun startMonitor() {
        if (monitor?.isAlive == true) return
        monitor = thread(name = "vm-monitor") {
            while (true) {
                val state = App.state()
                val phase = state.getString("phase")
                if (vmActive && phase in ENDED) vmActive = false
                if (!vmActive && !importing && !transferring) break
                val text = when {
                    importing -> importText()
                    transferring -> transferText()
                    else -> phaseText(phase)
                }
                if (text != lastText) {
                    lastText = text
                    getSystemService(NotificationManager::class.java).notify(NOTIFICATION, notification(text))
                }
                Thread.sleep(1000)
            }
            wakeLock?.release()
            wakeLock = null
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun importText(): String {
        val p = JSONObject(Native.importProgress())
        val total = p.optLong("totalBytes")
        val percent = if (total > 0) 100 * p.optLong("doneBytes") / total else 0
        return "Importing ${p.optString("name")}: $percent%"
    }

    private fun transferText(): String {
        val p = JSONObject(Native.shareProgress())
        val total = p.optLong("totalBytes")
        val percent = if (total > 0) 100 * p.optLong("doneBytes") / total else 0
        val verb = if (p.optString("op") == "import") "Receiving" else "Packing"
        return "$verb ${p.optString("name")}: $percent%"
    }

    /** Packs `vm` into `dest`, or into the app's share folder when `dest` is null. */
    private fun sendVm(vm: String, dest: Uri?): Transfer {
        return try {
            val (fd, path) = if (dest != null) {
                val pfd = contentResolver.openFileDescriptor(dest, "wt") ?: return Transfer("export", vm, "Cannot open the chosen file")
                pfd.detachFd() to ""
            } else {
                val path = Native.sharePath(shareDir(this).path, vm)
                val pfd = ParcelFileDescriptor.open(java.io.File(path), ParcelFileDescriptor.MODE_CREATE or ParcelFileDescriptor.MODE_TRUNCATE or ParcelFileDescriptor.MODE_WRITE_ONLY)
                pfd.detachFd() to path
            }
            val r = JSONObject(Native.shareExport(vm, fd))
            val error = r.optString("error")
            if (error.isNotEmpty()) {
                if (dest != null) runCatching { DocumentsContract.deleteDocument(contentResolver, dest) }
                if (path.isNotEmpty()) java.io.File(path).delete()
                Transfer("export", vm, if (error == "cancelled") "Sending cancelled" else error)
            } else {
                val t = Transfer("export", vm, "", path, r.optLong("bytes"))
                if (path.isNotEmpty()) notifyReady(t)
                t
            }
        } catch (e: Exception) {
            Transfer("export", vm, e.message ?: e.toString())
        }
    }

    private fun receiveVm(src: Uri?): Transfer {
        if (src == null) return Transfer("import", "", "No file was given")
        return try {
            val pfd = contentResolver.openFileDescriptor(src, "r") ?: return Transfer("import", "", "Cannot open the file")
            val r = JSONObject(Native.shareImport(pfd.detachFd()))
            val error = r.optString("error")
            Transfer("import", r.optString("name"), if (error == "cancelled") "Receiving cancelled" else error)
        } catch (e: Exception) {
            Transfer("import", "", e.message ?: e.toString())
        }
    }

    /**
     * Streams a `.vphone` file from a phone in File transfer mode straight into the importer: the
     * MTP read fills one end of a pipe while the importer reads the other, so nothing is staged.
     */
    private fun receiveVmFromUsb(deviceName: String, handle: Int): Transfer {
        val device = getSystemService(android.hardware.usb.UsbManager::class.java).deviceList[deviceName]
            ?: return Transfer("import", "", "The phone was unplugged before the copy started")
        return try {
            val mtp = UsbVmSource.open(this, device)
            try {
                val (read, write) = ParcelFileDescriptor.createReliablePipe()
                var mtpOk = false
                val reader = thread(name = "vm-usb-read") {
                    write.use { mtpOk = mtp.importFile(handle, it) }
                }
                val r = JSONObject(Native.shareImport(read.detachFd()))
                // An import that stopped early leaves the MTP read blocked on a full pipe: closing
                // the device ends it.
                if (reader.isAlive) mtp.close()
                reader.join(10_000)
                val error = r.optString("error")
                Transfer(
                    "import",
                    r.optString("name"),
                    when {
                        error == "cancelled" -> "Receiving cancelled"
                        error.isNotEmpty() && !mtpOk -> "$error (the USB copy from ${UsbVmSource.label(device)} also failed: check the cable and that the phone stays unlocked in File transfer mode)"
                        else -> error
                    },
                )
            } finally {
                mtp.close()
            }
        } catch (e: Exception) {
            Transfer("import", "", e.message ?: e.toString())
        }
    }

    /** "Ready to send", tapping which opens the share sheet even if the app is not on screen. */
    private fun notifyReady(t: Transfer) {
        val chooser = shareIntent(this, t) ?: return
        val pending = PendingIntent.getActivity(this, 3, chooser, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        getSystemService(NotificationManager::class.java).notify(
            NOTIFICATION_READY,
            Notification.Builder(this, App.CHANNEL)
                .setSmallIcon(R.drawable.ic_phone)
                .setContentTitle("${t.name} is ready to send")
                .setContentText("${t.bytes / (1024 * 1024)} MB. Tap to share it, with Quick Share or another app.")
                .setContentIntent(pending)
                .setAutoCancel(true)
                .build(),
        )
    }

    private fun import(tree: Uri) {
        lastImportResult = runCatching { copyTree(tree) }.getOrElse { it.message ?: it.toString() }
        Native.importAbort()
        runCatching { contentResolver.releasePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
        importing = false
    }

    private class Doc(val relativePath: String, val uri: Uri, val size: Long)

    /** The result to show the person: "" when the VM arrived. */
    private fun copyTree(tree: Uri): String {
        val rootId = DocumentsContract.getTreeDocumentId(tree)
        val folder = displayName(DocumentsContract.buildDocumentUriUsingTree(tree, rootId))
        val docs = ArrayList<Doc>()
        walk(tree, rootId, "", docs)
        val begun = JSONObject(Native.importBegin(folder, docs.sumOf { it.size }))
        begun.optString("error").let { if (it.isNotEmpty()) return it }
        for (doc in docs) {
            val pfd: ParcelFileDescriptor = contentResolver.openFileDescriptor(doc.uri, "r")
                ?: return "Cannot open ${doc.relativePath}"
            val error = Native.importFile(pfd.detachFd(), doc.relativePath)
            if (error.isNotEmpty()) return if (error == "cancelled") "Import cancelled" else "${doc.relativePath}: $error"
        }
        return Native.importFinish()
    }

    private fun displayName(uri: Uri): String =
        contentResolver.query(uri, arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME), null, null, null)
            ?.use { if (it.moveToFirst()) it.getString(0) else null } ?: "imported-vm"

    private fun walk(tree: Uri, parent: String, prefix: String, out: MutableList<Doc>) {
        val columns = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_SIZE,
        )
        val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parent)
        contentResolver.query(children, columns, null, null, null)?.use { c ->
            while (c.moveToNext()) {
                val id = c.getString(0)
                val name = c.getString(1)
                if (name.startsWith(".") || name.startsWith("serial.log")) continue
                if (c.getString(2) == DocumentsContract.Document.MIME_TYPE_DIR) {
                    walk(tree, id, "$prefix$name/", out)
                } else {
                    val size = if (c.isNull(3)) 0L else c.getLong(3)
                    out += Doc(prefix + name, DocumentsContract.buildDocumentUriUsingTree(tree, id), size)
                }
            }
        }
    }

    companion object {
        private const val NOTIFICATION = 1
        private const val ACTION_BOOT = "boot"
        private const val ACTION_STOP = "stop"
        private const val ACTION_IMPORT = "import"
        private const val ACTION_CANCEL_IMPORT = "cancel-import"
        private const val ACTION_SEND_VM = "send-vm"
        private const val ACTION_RECEIVE_VM = "receive-vm"
        private const val ACTION_CANCEL_TRANSFER = "cancel-transfer"
        private const val ACTION_RECEIVE_USB = "receive-usb"
        private const val EXTRA_USB_DEVICE = "usb-device"
        private const val EXTRA_USB_HANDLE = "usb-handle"
        private const val NOTIFICATION_READY = 2

        /** A send or receive that ended: `error` is "" when it worked. */
        class Transfer(val op: String, val name: String, val error: String, val path: String = "", val bytes: Long = 0)

        @Volatile var lastTransfer: Transfer? = null

        fun shareDir(context: Context) = java.io.File(context.externalCacheDir ?: context.cacheDir, "shared-vms")

        /** The share sheet for a packed VM. */
        fun shareIntent(context: Context, t: Transfer): Intent? {
            if (t.path.isEmpty()) return null
            val uri = androidx.core.content.FileProvider.getUriForFile(context, "${context.packageName}.logs", java.io.File(t.path))
            val send = Intent(Intent.ACTION_SEND)
                .setType("application/octet-stream")
                .putExtra(Intent.EXTRA_STREAM, uri)
                .putExtra(Intent.EXTRA_SUBJECT, "${t.name} (vphone VM)")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            return Intent.createChooser(send, "Send ${t.name}").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }

        fun sendVm(context: Context, vm: String, dest: Uri?) {
            context.startForegroundService(Intent(context, VmService::class.java).setAction(ACTION_SEND_VM).putExtra(EXTRA_VM, vm).setData(dest))
        }

        fun receiveVm(context: Context, src: Uri) {
            context.startForegroundService(Intent(context, VmService::class.java).setAction(ACTION_RECEIVE_VM).setData(src))
        }

        fun receiveVmFromUsb(context: Context, found: UsbVmSource.Found) {
            context.startForegroundService(
                Intent(context, VmService::class.java).setAction(ACTION_RECEIVE_USB)
                    .putExtra(EXTRA_USB_DEVICE, found.device).putExtra(EXTRA_USB_HANDLE, found.handle),
            )
        }

        fun cancelTransfer(context: Context) {
            context.startService(Intent(context, VmService::class.java).setAction(ACTION_CANCEL_TRANSFER))
        }
        private const val EXTRA_VM = "vm"
        private val ENDED = setOf("idle", "exited", "failed")

        /** The last import's outcome, once it ended: "" for success, null while none has ended. */
        @Volatile var lastImportResult: String? = null
        @Volatile var lastBootError: String? = null

        fun phaseText(phase: String) = when (phase) {
            "idle", "preparing" -> "Preparing"
            "booting" -> "iOS is booting"
            "unlocking" -> "Unlocking"
            "running" -> "Running"
            "stopping" -> "Shutting down"
            else -> phase
        }

        fun boot(context: Context, vm: String) {
            lastBootError = null
            context.startForegroundService(Intent(context, VmService::class.java).setAction(ACTION_BOOT).putExtra(EXTRA_VM, vm))
        }

        fun import(context: Context, tree: Uri) {
            lastImportResult = null
            context.startForegroundService(Intent(context, VmService::class.java).setAction(ACTION_IMPORT).setData(tree))
        }

        fun cancelImport() = Native.importCancel()
    }
}
