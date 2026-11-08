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
            else -> if (!vmActive && !importing) stopSelf()
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
                if (!vmActive && !importing) break
                val text = if (importing) importText() else phaseText(phase)
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
