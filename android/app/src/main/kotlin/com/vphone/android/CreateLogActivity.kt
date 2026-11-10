package com.vphone.android

import android.content.Context
import android.content.Intent
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.FileProvider
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.CreateLogActions
import com.vphone.android.ui.CreateLogScreen
import com.vphone.android.ui.CreateLogUi
import com.vphone.android.ui.DialogHost
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.VphoneTheme
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.text.DateFormat
import java.text.NumberFormat
import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

/** A step's log as it is written: Rust reads and indexes it, this shows the rows it is given. */
class CreateLogActivity : ComponentActivity() {
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private val dialogs = DialogHost()
    private lateinit var vm: String
    private lateinit var list: RecyclerView
    private lateinit var layoutManager: LinearLayoutManager
    private val adapter = RowAdapter()
    /** What is around the rows: the step, the filter, the counts. The rows are the adapter's. */
    private var ui by mutableStateOf(CreateLogUi())

    private var pinnedStep: String? = null
    private var stepKey = ""
    private var view = 0L
    private var running = false
    private var generation = -1L
    private var following = true
    private var dragging = false
    private var unseen = 0
    private var filter = FILTER_ALL
    private var search = ""
    private var allOpen = false
    private var steps = JSONArray()

    private val poll = Runnable { pollNow() }
    private val applySearch = Runnable { pushFilter() }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        vm = intent.getStringExtra(EXTRA_VM).orEmpty()
        pinnedStep = intent.getStringExtra(EXTRA_STEP)
        ui = CreateLogUi(vm = vm)

        list = RecyclerView(this)
        layoutManager = LinearLayoutManager(this)
        list.layoutManager = layoutManager
        list.adapter = adapter
        list.clipToPadding = false
        list.setPadding(0, 0, 0, dp(88))
        // A row whose time changes every second must not cross-fade.
        list.itemAnimator = null
        list.addOnScrollListener(object : RecyclerView.OnScrollListener() {
            override fun onScrollStateChanged(view: RecyclerView, state: Int) {
                if (state == RecyclerView.SCROLL_STATE_DRAGGING) {
                    dragging = true
                } else if (state == RecyclerView.SCROLL_STATE_IDLE) {
                    dragging = false
                    if (!view.canScrollVertically(1)) follow()
                }
            }

            override fun onScrolled(view: RecyclerView, dx: Int, dy: Int) {
                if (dragging && dy < 0) {
                    following = false
                    updateJump()
                }
            }
        })
        val actions = CreateLogActions(
            back = ::finish,
            copy = { copy(-1) },
            share = ::share,
            expand = {
                allOpen = !allOpen
                ui = ui.copy(allOpen = allOpen)
                val h = view
                val open = allOpen
                worker.execute { Native.createLogOpenAll(h, open) }
                pollSoon()
            },
            step = { i ->
                if (i == 0) {
                    pinnedStep = null
                    stepKey = ""
                } else {
                    pinnedStep = steps.getJSONObject(i - 1).getString("key")
                }
                pollSoon()
            },
            filter = { f ->
                // Tapping the filter that is on turns it off, as a chip does.
                filter = if (f == filter) FILTER_ALL else f
                ui = ui.copy(filter = filter)
                pushFilter()
            },
            search = { text ->
                search = text
                main.removeCallbacks(applySearch)
                main.postDelayed(applySearch, 300)
            },
            jump = {
                follow()
                scrollToEnd()
            },
        )
        setContent {
            VphoneTheme {
                val scheme = MaterialTheme.colorScheme
                val colors = RowColors(
                    tool = scheme.onSurface.toArgb(),
                    guest = scheme.tertiary.toArgb(),
                    usb = scheme.onSurfaceVariant.toArgb(),
                    error = scheme.error.toArgb(),
                    accent = scheme.primary.toArgb(),
                )
                SideEffect { adapter.setColors(colors) }
                CreateLogScreen(ui, actions) { modifier -> AndroidView(factory = { list }, modifier = modifier) }
                dialogs.current?.let { spec ->
                    AppDialog(spec, onCopy = { App.copy(this, it) }, onDismiss = { dialogs.current = null })
                }
            }
        }
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density).toInt()

    override fun onStart() {
        super.onStart()
        running = true
        pollNow()
    }

    override fun onStop() {
        running = false
        main.removeCallbacks(poll)
        main.removeCallbacks(applySearch)
        super.onStop()
    }

    override fun onDestroy() {
        val h = view
        view = 0
        worker.execute {
            if (h != 0L) Native.createLogClose(h)
        }
        worker.shutdown()
        super.onDestroy()
    }

    private fun pollSoon() {
        main.removeCallbacks(poll)
        main.post(poll)
    }

    private fun schedule(delay: Long) {
        main.removeCallbacks(poll)
        if (running) main.postDelayed(poll, delay)
    }

    private fun pollNow() {
        if (!running) return
        val name = vm
        worker.execute {
            val status = runCatching { JSONObject(Native.createStatus(name)) }.getOrNull()
            main.post {
                if (running) {
                    if (status != null) onStatus(status) else schedule(POLL_MS)
                }
            }
        }
    }

    private fun onStatus(s: JSONObject) {
        steps = s.optJSONArray("steps") ?: JSONArray()
        val key = pinnedStep ?: s.optString("step").ifEmpty { lastStartedStep() }
        val step = stepNamed(key)
        if (step == null) {
            showWaiting()
            schedule(POLL_MS)
            return
        }
        if (key != stepKey) switchTo(key, step.getString("log"))
        ui = ui.copy(
            stepLabel = if (pinnedStep == null) "Current step: ${step.optString("title", key)}" else step.optString("title", key),
            steps = (0 until steps.length()).map { steps.getJSONObject(it) }
                .map { "${stateMark(it.optString("state"))} ${it.optString("title")}" },
        )
        val stepState = step.optString("state")
        val runState = if (stepState == "running") s.optString("state") else stepState
        val paused = s.optBoolean("paused")
        val h = view
        worker.execute {
            val result = runCatching { JSONObject(Native.createLogSync(h, runState, paused)) }.getOrNull()
            main.post {
                if (h == view && running) {
                    result?.let(::applySummary)
                    schedule(if (result?.optBoolean("busy") == true) 0L else POLL_MS)
                }
            }
        }
    }

    private fun lastStartedStep(): String {
        for (i in steps.length() - 1 downTo 0) {
            val st = steps.getJSONObject(i)
            if (st.optString("state") != "pending") return st.getString("key")
        }
        return if (steps.length() > 0) steps.getJSONObject(0).getString("key") else ""
    }

    private fun stepNamed(key: String): JSONObject? =
        (0 until steps.length()).map { steps.getJSONObject(it) }.firstOrNull { it.optString("key") == key }

    private fun showWaiting() {
        ui = ui.copy(empty = "No step has started yet.")
    }

    private fun switchTo(key: String, path: String) {
        val old = view
        view = Native.createLogOpen(path)
        stepKey = key
        generation = -1
        adapter.reset()
        unseen = 0
        following = true
        updateJump()
        val h = view
        val (f, q) = filter to search
        worker.execute {
            if (old != 0L) Native.createLogClose(old)
            Native.createLogFilter(h, f, q)
        }
    }

    private fun pushFilter() {
        val (h, f, q) = Triple(view, filter, search)
        if (h == 0L) return
        worker.execute { Native.createLogFilter(h, f, q) }
        pollSoon()
    }

    private fun stateMark(state: String) = when (state) {
        "done" -> "✓"
        "running" -> "▶"
        "failed" -> "✕"
        "paused" -> "⏸"
        "stopped" -> "■"
        else -> "·"
    }

    private fun applySummary(s: JSONObject) {
        val gen = s.optLong("generation")
        val rows = s.optInt("rows")
        val old = adapter.count
        when {
            gen != generation || rows < old -> {
                generation = gen
                adapter.setAll(rows)
                unseen = 0
            }
            rows > old -> {
                adapter.append(old, rows)
                if (!following) unseen += rows - old
            }
        }
        adapter.refreshVisible(layoutManager)
        if (following && rows > 0) scrollToEnd()
        updateJump()

        val lines = NUMBER.format(s.optLong("lines"))
        val matches = if (s.isNull("matches")) null else NUMBER.format(s.optLong("matches"))
        ui = ui.copy(
            summary = buildString {
                append(if (matches != null) "$matches of $lines lines" else "$lines lines")
                append(" · ${s.optInt("stages")} stages")
                if (s.optBoolean("busy")) append(" · reading…")
            },
            empty = when {
                rows != 0 -> null
                matches != null -> "Nothing matches."
                s.optBoolean("busy") -> "Reading the log…"
                else -> "This step has not written anything yet."
            },
        )
    }

    private fun follow() {
        following = true
        unseen = 0
        updateJump()
    }

    private fun updateJump() {
        val show = !following && adapter.count > 0
        ui = ui.copy(jump = if (!show) null else if (unseen > 0) "Jump to latest ($unseen new)" else "Jump to latest")
    }

    private fun scrollToEnd() {
        if (adapter.itemCount > 0) layoutManager.scrollToPosition(adapter.itemCount - 1)
    }

    private fun copy(stage: Int) {
        val h = view
        if (h == 0L) return
        worker.execute {
            val text = Native.createLogCopy(h, stage)
            main.post { App.copy(this, text) }
        }
    }

    private fun share() {
        val h = view
        if (h == 0L) return
        val dir = File(cacheDir, "shared-logs").apply { mkdirs() }
        val file = File(dir, "$vm-$stepKey.log")
        worker.execute {
            val result = Native.createLogExport(h, file.path)
            main.post {
                if (result.startsWith("error:")) {
                    dialogs.show(DialogSpec.error("The log could not be shared", result))
                } else {
                    val uri = FileProvider.getUriForFile(this, "$packageName.logs", file)
                    val send = Intent(Intent.ACTION_SEND)
                        .setType("text/plain")
                        .putExtra(Intent.EXTRA_STREAM, uri)
                        .putExtra(Intent.EXTRA_SUBJECT, "$vm $stepKey log ($result)")
                        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    startActivity(Intent.createChooser(send, "Share the log"))
                }
            }
        }
    }

    private fun toggle(stage: Int) {
        val h = view
        worker.execute { Native.createLogToggle(h, stage) }
        pollSoon()
    }

    private fun timeText(ms: Long): String {
        if (ms == 0L) return "time not recorded"
        val date = Date(ms)
        val same = Calendar.getInstance().get(Calendar.DAY_OF_YEAR) ==
            Calendar.getInstance().apply { time = date }.get(Calendar.DAY_OF_YEAR)
        return if (same) CLOCK.format(date) else DAY_CLOCK.format(date)
    }

    private fun duration(secs: Long): String {
        if (secs < 0) return ""
        val (h, m, s) = Triple(secs / 3600, (secs % 3600) / 60, secs % 60)
        return when {
            h > 0 -> String.format(Locale.US, "%d h %02d min", h, m)
            m > 0 -> String.format(Locale.US, "%d min %02d s", m, s)
            else -> "$s s"
        }
    }

    private class RowColors(val tool: Int, val guest: Int, val usb: Int, val error: Int, val accent: Int)

    private class RowHolder(root: View, val line: TextView?, val title: TextView?, val detail: TextView?, val copy: TextView?) :
        RecyclerView.ViewHolder(root)

    /** A line of the log: monospace, and selectable. */
    private fun lineRow(): RowHolder {
        val line = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextIsSelectable(true)
            setPadding(dp(12), dp(1), dp(12), dp(1))
            layoutParams = RecyclerView.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        }
        return RowHolder(line, line, null, null, null)
    }

    /** A stage's heading: its name and times, which open and close it, and Copy for its lines. */
    private fun stageRow(): RowHolder {
        val ripple = TypedValue().also { theme.resolveAttribute(android.R.attr.selectableItemBackground, it, true) }.resourceId
        val title = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTypeface(typeface, Typeface.BOLD)
        }
        val detail = TextView(this).apply { setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f) }
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(title)
            addView(detail)
        }
        val copy = TextView(this).apply {
            text = "Copy"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTypeface(typeface, Typeface.BOLD)
            gravity = Gravity.CENTER
            minHeight = dp(48)
            minWidth = dp(64)
            setPadding(dp(12), 0, dp(12), 0)
            setBackgroundResource(ripple)
            isClickable = true
            isFocusable = true
        }
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), dp(6), dp(4), dp(6))
            setBackgroundResource(ripple)
            isClickable = true
            isFocusable = true
            layoutParams = RecyclerView.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
            addView(words, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(copy, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        }
        return RowHolder(row, null, title, detail, copy)
    }

    private inner class RowAdapter : RecyclerView.Adapter<RowHolder>() {
        var count = 0
            private set
        private val pages = object : LinkedHashMap<Int, List<JSONObject>>(64, 0.75f, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Int, List<JSONObject>>) = size > MAX_PAGES
        }
        private val bound = HashMap<Int, String>()
        private var colors = RowColors(0, 0, 0, 0, 0)

        /** The theme's colours, which change with light and dark. */
        fun setColors(now: RowColors) {
            val same = now.tool == colors.tool && now.guest == colors.guest && now.usb == colors.usb &&
                now.error == colors.error && now.accent == colors.accent
            colors = now
            if (!same) notifyDataSetChanged()
        }

        fun reset() {
            pages.clear()
            bound.clear()
            count = 0
            notifyDataSetChanged()
        }

        fun setAll(rows: Int) {
            pages.clear()
            bound.clear()
            count = rows
            notifyDataSetChanged()
        }

        fun append(old: Int, rows: Int) {
            pages.remove(old / PAGE)
            count = rows
            notifyItemRangeInserted(old, rows - old)
        }

        /** The rows on screen change as they run: a stage's time, then its state. */
        fun refreshVisible(lm: LinearLayoutManager) {
            val first = lm.findFirstVisibleItemPosition()
            val last = lm.findLastVisibleItemPosition()
            if (first < 0 || last < 0) return
            for (page in first / PAGE..last / PAGE) pages.remove(page)
            for (pos in first..last) {
                val now = row(pos)?.toString() ?: continue
                if (bound[pos] != now) notifyItemChanged(pos)
            }
        }

        private fun row(pos: Int): JSONObject? {
            if (pos >= count || view == 0L) return null
            val page = pos / PAGE
            val rows = pages.getOrPut(page) {
                val a = JSONArray(Native.createLogRows(view, page * PAGE, PAGE))
                (0 until a.length()).map { a.getJSONObject(it) }
            }
            return rows.getOrNull(pos % PAGE)
        }

        override fun getItemCount() = count

        override fun getItemViewType(position: Int) = if (row(position)?.optString("k") == "h") 1 else 0

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): RowHolder =
            if (viewType == 1) stageRow() else lineRow()

        override fun onBindViewHolder(holder: RowHolder, position: Int) {
            val r = row(position) ?: return
            bound[position] = r.toString()
            if (r.optString("k") == "h") bindStage(holder, r) else bindLine(holder, r)
        }

        private fun bindLine(holder: RowHolder, r: JSONObject) {
            val text = holder.line ?: return
            text.text = r.optString("text")
            text.setTextColor(
                when {
                    r.optBoolean("err") -> colors.error
                    r.optInt("src") == 1 -> colors.guest
                    r.optInt("src") == 2 -> colors.usb
                    else -> colors.tool
                },
            )
        }

        private fun bindStage(holder: RowHolder, r: JSONObject) {
            val state = r.optString("state")
            val open = r.optBoolean("open")
            val stage = r.optInt("stage")
            holder.title?.text = "${if (open) "▾" else "▸"} ${stateMark(state)} ${r.optString("title")}"
            val secs = r.optLong("secs", -1)
            val parts = buildList {
                add(if (r.optLong("startMs") != 0L) "started ${timeText(r.optLong("startMs"))}" else "time not recorded")
                if (secs >= 0) add((if (state == "running" || state == "paused") "running " else "ran ") + duration(secs))
                val lines = r.optLong("lines")
                add("${NUMBER.format(lines)} ${if (lines == 1L) "line" else "lines"}")
                val errors = r.optLong("errors")
                if (errors > 0) add("$errors with errors")
            }
            holder.detail?.text = parts.joinToString(" · ")
            holder.title?.setTextColor(colors.tool)
            holder.copy?.setTextColor(colors.accent)
            holder.copy?.contentDescription = "Copy the lines of ${r.optString("title")}"
            holder.detail?.setTextColor(if (r.optLong("errors") > 0) colors.error else colors.usb)
            holder.itemView.setPaddingRelative(
                ((8 + 16 * r.optInt("level")) * resources.displayMetrics.density).toInt(),
                holder.itemView.paddingTop,
                holder.itemView.paddingEnd,
                holder.itemView.paddingBottom,
            )
            holder.itemView.setOnClickListener { toggle(stage) }
            holder.copy?.setOnClickListener { copy(stage) }
        }

        override fun onViewAttachedToWindow(holder: RowHolder) {
            // A recycled selectable TextView otherwise refuses selection after re-attaching.
            holder.line?.let {
                it.isEnabled = false
                it.isEnabled = true
            }
        }
    }

    companion object {
        const val EXTRA_VM = "vm"
        const val EXTRA_STEP = "step"
        private const val POLL_MS = 1000L
        private const val PAGE = 128
        private const val MAX_PAGES = 96
        private const val FILTER_ALL = 0
        private const val FILTER_TOOL = 1
        private const val FILTER_VM = 2
        private const val FILTER_USB = 3
        private const val FILTER_ERRORS = 4
        private val NUMBER: NumberFormat = NumberFormat.getIntegerInstance()
        private val CLOCK: DateFormat = SimpleDateFormat("HH:mm:ss", Locale.getDefault())
        private val DAY_CLOCK: DateFormat = SimpleDateFormat("MMM d HH:mm:ss", Locale.getDefault())

        fun open(context: Context, vm: String, step: String? = null) {
            context.startActivity(
                Intent(context, CreateLogActivity::class.java)
                    .putExtra(EXTRA_VM, vm)
                    .putExtra(EXTRA_STEP, step),
            )
        }
    }
}
