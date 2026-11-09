package com.vphone.android

import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.TextWatcher
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.PopupMenu
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.chip.ChipGroup
import com.google.android.material.color.MaterialColors
import com.google.android.material.floatingactionbutton.ExtendedFloatingActionButton
import com.google.android.material.textfield.TextInputEditText
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
class CreateLogActivity : AppCompatActivity() {
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var vm: String
    private lateinit var list: RecyclerView
    private lateinit var layoutManager: LinearLayoutManager
    private lateinit var jump: ExtendedFloatingActionButton
    private lateinit var summary: TextView
    private lateinit var empty: TextView
    private lateinit var stepButton: Button
    private lateinit var expandButton: Button
    private val adapter = RowAdapter()

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
        super.onCreate(savedInstanceState)
        vm = intent.getStringExtra(EXTRA_VM).orEmpty()
        pinnedStep = intent.getStringExtra(EXTRA_STEP)
        setContentView(R.layout.create_log)
        App.fitSystemBars(findViewById(R.id.log_root))

        summary = findViewById(R.id.log_summary)
        empty = findViewById(R.id.log_empty)
        jump = findViewById(R.id.log_jump)
        stepButton = findViewById(R.id.log_step)
        expandButton = findViewById(R.id.log_expand)
        list = findViewById(R.id.log_list)
        layoutManager = LinearLayoutManager(this)
        list.layoutManager = layoutManager
        list.adapter = adapter
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
        jump.setOnClickListener {
            follow()
            scrollToEnd()
        }
        findViewById<Button>(R.id.log_back).setOnClickListener { finish() }
        findViewById<Button>(R.id.log_copy).setOnClickListener { copy(-1) }
        findViewById<Button>(R.id.log_share).setOnClickListener { share() }
        expandButton.setOnClickListener {
            allOpen = !allOpen
            expandButton.text = if (allOpen) "Close all" else "Open all"
            val h = view
            val open = allOpen
            worker.execute { Native.createLogOpenAll(h, open) }
            pollSoon()
        }
        stepButton.setOnClickListener { chooseStep() }
        findViewById<ChipGroup>(R.id.log_filters).setOnCheckedStateChangeListener { _, ids ->
            filter = when (ids.firstOrNull()) {
                R.id.filter_tool -> FILTER_TOOL
                R.id.filter_vm -> FILTER_VM
                R.id.filter_usb -> FILTER_USB
                R.id.filter_errors -> FILTER_ERRORS
                else -> FILTER_ALL
            }
            pushFilter()
        }
        findViewById<TextInputEditText>(R.id.log_search).addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit
            override fun afterTextChanged(s: Editable?) {
                search = s?.toString().orEmpty()
                main.removeCallbacks(applySearch)
                main.postDelayed(applySearch, 300)
            }
        })
    }

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
        stepButton.text = if (pinnedStep == null) "Current step: ${step.optString("title", key)}" else step.optString("title", key)
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
        empty.visibility = View.VISIBLE
        empty.text = "No step has started yet."
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

    private fun chooseStep() {
        val menu = PopupMenu(this, stepButton)
        menu.menu.add(0, 0, 0, "Current step (follows the creation)")
        for (i in 0 until steps.length()) {
            val st = steps.getJSONObject(i)
            menu.menu.add(0, i + 1, i + 1, "${stateMark(st.optString("state"))} ${st.optString("title")}")
        }
        menu.setOnMenuItemClickListener { item ->
            if (item.itemId == 0) {
                pinnedStep = null
                stepKey = ""
            } else {
                pinnedStep = steps.getJSONObject(item.itemId - 1).getString("key")
            }
            pollSoon()
            true
        }
        menu.show()
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
        summary.text = buildString {
            append(if (matches != null) "$matches of $lines lines" else "$lines lines")
            append(" · ${s.optInt("stages")} stages")
            if (s.optBoolean("busy")) append(" · reading…")
        }
        empty.visibility = if (rows == 0) View.VISIBLE else View.GONE
        empty.text = when {
            matches != null -> "Nothing matches."
            s.optBoolean("busy") -> "Reading the log…"
            else -> "This step has not written anything yet."
        }
    }

    private fun follow() {
        following = true
        unseen = 0
        updateJump()
    }

    private fun updateJump() {
        val show = !following && adapter.count > 0
        jump.visibility = if (show) View.VISIBLE else View.GONE
        if (show) jump.text = if (unseen > 0) "Jump to latest ($unseen new)" else "Jump to latest"
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
                    Toast.makeText(this, result, Toast.LENGTH_LONG).show()
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

    private class RowHolder(root: View) : RecyclerView.ViewHolder(root) {
        val line: TextView? = root.findViewById(R.id.line)
        val title: TextView? = root.findViewById(R.id.stage_title)
        val detail: TextView? = root.findViewById(R.id.stage_detail)
        val copy: Button? = root.findViewById(R.id.stage_copy)
    }

    private inner class RowAdapter : RecyclerView.Adapter<RowHolder>() {
        var count = 0
            private set
        private val pages = object : LinkedHashMap<Int, List<JSONObject>>(64, 0.75f, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Int, List<JSONObject>>) = size > MAX_PAGES
        }
        private val bound = HashMap<Int, String>()

        private val toolColor by lazy { MaterialColors.getColor(list, com.google.android.material.R.attr.colorOnSurface) }
        private val guestColor by lazy { MaterialColors.getColor(list, com.google.android.material.R.attr.colorTertiary) }
        private val usbColor by lazy { MaterialColors.getColor(list, com.google.android.material.R.attr.colorOnSurfaceVariant) }
        private val errorColor by lazy { MaterialColors.getColor(list, androidx.appcompat.R.attr.colorError) }

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

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): RowHolder {
            val inflater = LayoutInflater.from(parent.context)
            val layout = if (viewType == 1) R.layout.create_log_stage else R.layout.create_log_line
            return RowHolder(inflater.inflate(layout, parent, false))
        }

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
                    r.optBoolean("err") -> errorColor
                    r.optInt("src") == 1 -> guestColor
                    r.optInt("src") == 2 -> usbColor
                    else -> toolColor
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
            holder.detail?.setTextColor(if (r.optLong("errors") > 0) errorColor else usbColor)
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
