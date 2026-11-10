package com.vphone.android

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.core.content.FileProvider
import java.io.File
import java.io.RandomAccessFile
import java.util.concurrent.Executors

/**
 * The running VM's log, live: QEMU's output and the status line every 30 s. Copy takes what is
 * shown; Share sends a file with more of the log and this app's logcat, which has the GPU
 * driver's messages.
 */
class VmLogActivity : Activity() {
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var scroll: ScrollView
    private lateinit var text: TextView
    private lateinit var heading: TextView
    private lateinit var followButton: Button
    private var path = ""
    private var shown = ""
    private var following = true

    private val poll = object : Runnable {
        override fun run() {
            load()
            main.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        path = intent.getStringExtra(EXTRA_PATH).orEmpty().ifEmpty { App.state().optString("logPath") }
        heading = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 16f
            setPadding(8, 8, 8, 8)
        }
        text = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            textSize = 10f
            setTextColor(0xFFE0E0E0.toInt())
            setTextIsSelectable(true)
            setPadding(8, 8, 8, 8)
        }
        scroll = ScrollView(this).apply {
            addView(text)
            setOnScrollChangeListener { v, _, y, _, _ ->
                val bottom = (v as ScrollView).getChildAt(0).height - v.height
                val atEnd = y >= bottom - 48
                if (atEnd != following) {
                    following = atEnd
                    followButton.visibility = if (following) View.GONE else View.VISIBLE
                }
            }
        }
        followButton = button("Jump to the end") {
            following = true
            followButton.visibility = View.GONE
            scrollToEnd()
        }.apply { visibility = View.GONE }
        val actions = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            val weight = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            addView(button("Copy") { App.copy(this@VmLogActivity, shown) }, weight)
            addView(button("Share") { share() }, weight)
            addView(button("Back") { finish() }, weight)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            setPadding(16, 16, 16, 16)
            addView(heading)
            addView(scroll, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(followButton)
            addView(actions)
        }
        App.fitSystemBars(root)
        setContentView(root)
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

    private fun button(label: String, action: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        setOnClickListener { action() }
    }

    private fun load() {
        if (path.isEmpty()) {
            heading.text = "No VM has run since the app started"
            return
        }
        worker.execute {
            val log = Native.logTail(path, SHOWN_BYTES).ifEmpty { "(the log is empty)" }
            main.post { show(log) }
        }
    }

    private fun show(log: String) {
        val state = App.state()
        val phase = state.optString("phase")
        val started = state.optLong("startedMs")
        val elapsed = if (started > 0 && phase in UP) " for ${clock((System.currentTimeMillis() - started) / 1000)}" else ""
        heading.text = "${state.optString("vm").ifEmpty { File(path).parentFile?.name.orEmpty() }}: " +
            "${VmService.phaseText(phase)}$elapsed"
        if (log == shown || !following && shown.isNotEmpty()) return
        shown = log
        text.text = log
        if (following) scrollToEnd()
    }

    private fun scrollToEnd() = scroll.post { scroll.fullScroll(View.FOCUS_DOWN) }

    private fun share() {
        val dir = File(cacheDir, "shared-logs").apply { mkdirs() }
        val vm = App.state().optString("vm").ifEmpty { "vm" }
        val file = File(dir, "$vm-boot.log")
        Toast.makeText(this, "Preparing the log", Toast.LENGTH_SHORT).show()
        worker.execute {
            val result = runCatching { writeShared(file) }
            main.post {
                result.exceptionOrNull()?.let {
                    Toast.makeText(this, "Cannot prepare the log: ${it.message}", Toast.LENGTH_LONG).show()
                    return@post
                }
                val uri = FileProvider.getUriForFile(this, "$packageName.logs", file)
                val send = Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_STREAM, uri)
                    .putExtra(Intent.EXTRA_SUBJECT, "$vm boot log")
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
