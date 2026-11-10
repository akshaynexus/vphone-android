package com.vphone.android.ui

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.FloatingToolbarDefaults
import androidx.compose.material3.HorizontalFloatingToolbar
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * A VM's log as it grows. It follows the end until the reader scrolls up; then it stays where
 * they are, and a button brings the end back.
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun VmLogScreen(
    heading: String,
    log: String,
    following: Boolean,
    onFollow: (Boolean) -> Unit,
    onCopy: () -> Unit,
    onShare: () -> Unit,
    onBack: () -> Unit,
) {
    val scroll = rememberScrollState()
    LaunchedEffect(log, scroll.maxValue, following) {
        if (following) scroll.scrollTo(scroll.maxValue)
    }
    // Only the reader's own scrolling changes whether the end is followed: the text growing does not.
    LaunchedEffect(scroll) {
        snapshotFlow { scroll.isScrollInProgress to (scroll.value >= scroll.maxValue - 48) }
            .collect { (moving, atEnd) -> if (moving) onFollow(atEnd) }
    }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(heading, maxLines = 2, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.titleMedium) },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
            )
        },
    ) { inner ->
        Box(Modifier.fillMaxSize().padding(inner)) {
            SelectionContainer {
                Text(
                    log,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 10.sp,
                    lineHeight = 13.sp,
                    modifier = Modifier.fillMaxSize().verticalScroll(scroll).padding(start = 12.dp, end = 12.dp, bottom = 96.dp),
                )
            }
            HorizontalFloatingToolbar(
                expanded = true,
                colors = FloatingToolbarDefaults.vibrantFloatingToolbarColors(),
                modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 16.dp),
            ) {
                TextButton(onClick = onCopy) {
                    Icon(VIcons.Copy, contentDescription = null, modifier = Modifier.size(18.dp))
                    Text("Copy", Modifier.padding(start = 6.dp))
                }
                TextButton(onClick = onShare) {
                    Icon(Icons.Filled.Share, contentDescription = null, modifier = Modifier.size(18.dp))
                    Text("Share", Modifier.padding(start = 6.dp))
                }
                if (!following) {
                    FilledTonalButton(onClick = { onFollow(true) }) { Text("Jump to the end") }
                }
            }
        }
    }
}

data class CreateLogUi(
    val vm: String = "",
    val stepLabel: String = "Current step",
    /** The steps as the chooser lists them, in order; the first entry of the menu is "current". */
    val steps: List<String> = emptyList(),
    val filter: Int = 0,
    val summary: String = "",
    /** Said in place of the rows when there are none. */
    val empty: String? = null,
    val allOpen: Boolean = false,
    /** The "jump to latest" button's words; null while the end is followed. */
    val jump: String? = null,
)

class CreateLogActions(
    val back: () -> Unit = {},
    val copy: () -> Unit = {},
    val share: () -> Unit = {},
    val expand: () -> Unit = {},
    /** 0 is "the current step"; `n` is the n-th step of the list. */
    val step: (Int) -> Unit = {},
    val filter: (Int) -> Unit = {},
    val search: (String) -> Unit = {},
    val jump: () -> Unit = {},
)

/** The creation log's screen around its rows, which `rows` draws (a list paged from Rust). */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun CreateLogScreen(ui: CreateLogUi, actions: CreateLogActions, rows: @Composable (Modifier) -> Unit) {
    var search by rememberSaveable { mutableStateOf("") }
    var choosing by remember { mutableStateOf(false) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Log · ${ui.vm}", maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    IconButton(onClick = actions.back) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
                actions = {
                    IconButton(onClick = actions.copy) { Icon(VIcons.Copy, contentDescription = "Copy what is shown") }
                    IconButton(onClick = actions.share) { Icon(Icons.Filled.Share, contentDescription = "Share the log") }
                },
            )
        },
        floatingActionButton = {
            ui.jump?.let { label -> ExtendedFloatingActionButton(onClick = actions.jump) { Text(label) } }
        },
    ) { inner ->
        Column(Modifier.fillMaxSize().padding(inner)) {
            Column(Modifier.padding(horizontal = 12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Box {
                    FilledTonalButton(onClick = { choosing = true }, modifier = Modifier.fillMaxWidth()) {
                        Text(ui.stepLabel, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                        Icon(VIcons.Expand, contentDescription = "Choose a step")
                    }
                    DropdownMenu(expanded = choosing, onDismissRequest = { choosing = false }) {
                        (listOf("Current step (follows the creation)") + ui.steps).forEachIndexed { i, label ->
                            DropdownMenuItem(
                                text = { Text(label) },
                                onClick = {
                                    choosing = false
                                    actions.step(i)
                                },
                            )
                        }
                    }
                }
                OutlinedTextField(
                    value = search,
                    onValueChange = {
                        search = it
                        actions.search(it)
                    },
                    label = { Text("Search this log") },
                    leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    listOf("All", "Tool", "VM", "USB", "Errors").forEachIndexed { i, label ->
                        FilterChip(selected = ui.filter == i, onClick = { actions.filter(i) }, label = { Text(label) })
                    }
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        ui.summary,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(onClick = actions.expand) { Text(if (ui.allOpen) "Close all" else "Open all") }
                }
            }
            Box(Modifier.weight(1f).fillMaxWidth()) {
                rows(Modifier.fillMaxSize())
                ui.empty?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.align(Alignment.Center).padding(24.dp),
                    )
                }
            }
        }
    }
}
