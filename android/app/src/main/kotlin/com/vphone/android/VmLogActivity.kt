package com.vphone.android

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.content.FileProvider
import com.vphone.android.ui.VmLogScreen
import com.vphone.android.ui.VphoneTheme
import java.io.File
import java.io.RandomAccessFile
import java.util.concurrent.Executors

/**
 * The running VM's log, live: QEMU's output and the status line every 30 s. Copy takes what is
 * shown; Share sends a file with more of the log and this app's logcat, which has the GPU
 * driver's messages, and with the VM's GPU diagnostics on, pictures of the surface behind the
 * frame on screen ([FrameDumps]).
 */
class VmLogActivity : ComponentActivity() {
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private var path = ""
    private var heading by mutableStateOf("")
    private var shown by mutableStateOf("")
    private var following by mutableStateOf(true)

    private val poll = object : Runnable {
        override fun run() {
            load()
            main.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        path = intent.getStringExtra(EXTRA_PATH).orEmpty().ifEmpty { App.state().optString("logPath") }
        setContent {
            VphoneTheme {
                VmLogScreen(
                    heading = heading,
                    log = shown,
                    following = following,
                    onFollow = { following = it },
                    onCopy = { App.copy(this, shown) },
                    onShare = ::share,
                    onBack = ::finish,
                )
            }
        }
    }

    override fun onStart() {
        super.onStart()
        main.post(poll)
    }

    override fun onStop() {
        main.removeCallbacks(poll)
        super.onStop()
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    private fun load() {
        if (path.isEmpty()) {
            heading = "No VM has run since the app started"
            return
        }
        worker.execute {
            val log = Native.logTail(path, SHOWN_BYTES).ifEmpty { "(the log is empty)" }
            main.post { if (!isDestroyed) show(log) }
        }
    }

    private fun show(log: String) {
        val state = App.state()
        val phase = state.optString("phase")
        val started = state.optLong("startedMs")
        val folder = File(path).parentFile?.name.orEmpty()
        // A log that is not the VM's in this process says whose file it is, not a phase.
        heading = if (state.optString("logPath") == path || state.optString("vm") == folder) {
            val elapsed = if (started > 0 && phase in UP) " for ${clock((System.currentTimeMillis() - started) / 1000)}" else ""
            "${state.optString("vm").ifEmpty { folder }}: ${VmService.phaseText(phase)}$elapsed"
        } else {
            "$folder: ${File(path).name}"
        }
        if (log == shown || !following && shown.isNotEmpty()) return
        shown = log
    }

    private fun share() {
        val dir = File(cacheDir, "shared-logs").apply { mkdirs() }
        val vm = App.state().optString("vm").ifEmpty { "vm" }
        val file = File(dir, "$vm-boot.log")
        Toast.makeText(this, "Preparing the log", Toast.LENGTH_SHORT).show()
        worker.execute {
            val result = runCatching {
                writeShared(file)
                FrameDumps.toPngs(File(filesDir, FrameDumps.DIR), dir)
            }
            main.post {
                result.exceptionOrNull()?.let {
                    Toast.makeText(this, "Cannot prepare the log: ${it.message}", Toast.LENGTH_LONG).show()
                    return@post
                }
                val uri = FileProvider.getUriForFile(this, "$packageName.logs", file)
                val frames = result.getOrDefault(emptyList())
                    .map { FileProvider.getUriForFile(this, "$packageName.logs", it) }
                val send = if (frames.isEmpty()) {
                    Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_STREAM, uri)
                } else {
                    // The log and the pictures of the guest's surface, as the GPU diagnostics saved them.
                    Intent(Intent.ACTION_SEND_MULTIPLE).setType("*/*")
                        .putParcelableArrayListExtra(Intent.EXTRA_STREAM, ArrayList(listOf(uri) + frames))
                }
                send.putExtra(Intent.EXTRA_SUBJECT, "$vm boot log")
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                startActivity(Intent.createChooser(send, "Share the log"))
            }
        }
    }

    /** The log's last [SHARED_BYTES], then this process's logcat, which only it may read. */
    private fun writeShared(out: File) {
        out.outputStream().buffered().use { w ->
            val log = File(path)
            if (log.exists()) {
                RandomAccessFile(log, "r").use { f ->
                    val start = (f.length() - SHARED_BYTES).coerceAtLeast(0)
                    if (start > 0) w.write("(the first $start bytes are left out)\n".toByteArray())
                    f.seek(start)
                    val buf = ByteArray(1 shl 16)
                    while (true) {
                        val n = f.read(buf)
                        if (n <= 0) break
                        w.write(buf, 0, n)
                    }
                }
            }
            w.write("\n===== logcat of this app (pid ${Process.myPid()}) =====\n".toByteArray())
            val logcat = ProcessBuilder("logcat", "-d", "-v", "threadtime", "-t", LOGCAT_LINES.toString(), "--pid", Process.myPid().toString())
                .redirectErrorStream(true)
                .start()
            logcat.inputStream.use { it.copyTo(w) }
            logcat.waitFor()
        }
    }

    companion object {
        const val EXTRA_PATH = "path"
        private const val SHOWN_BYTES = 96L * 1024
        private const val SHARED_BYTES = 8L * 1024 * 1024
        private const val LOGCAT_LINES = 5000
        private val UP = setOf("booting", "unlocking", "running", "stopping")

        fun clock(secs: Long) = "%d:%02d".format(secs / 60, secs % 60)

        fun open(activity: Activity, path: String = "") {
            activity.startActivity(Intent(activity, VmLogActivity::class.java).putExtra(EXTRA_PATH, path))
        }
    }
}
