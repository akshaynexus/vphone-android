package com.vphone.android.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeFlexibleTopAppBar
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.LoadingIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp

class StorageButton(val label: String, val enabled: Boolean, val danger: Boolean = false, val action: () -> Unit)

/** One thing the app keeps: what it is, why it cannot be removed now if it cannot, and what can be done. */
class StorageRow(val title: String, val detail: String, val blocked: String = "", val buttons: List<StorageButton> = emptyList())

class StorageSection(val title: String, val empty: String, val rows: List<StorageRow>, val all: StorageButton? = null)

class StorageUi(
    /** Null until the first measurement is back. */
    val summary: String? = null,
    val used: Float = 0f,
    val sections: List<StorageSection> = emptyList(),
    val resetText: String = "",
    val reset: StorageButton? = null,
    val progress: ProgressUi? = null,
    /** An action is running or being prepared: nothing else can start. */
    val busy: Boolean = false,
)

@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun StorageScreen(ui: StorageUi, onBack: () -> Unit) {
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            LargeFlexibleTopAppBar(
                title = { Text("Storage") },
                subtitle = { Text("What vphone keeps on this phone") },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
                scrollBehavior = scroll,
            )
        },
    ) { inner ->
        Box(Modifier.fillMaxSize().padding(inner), contentAlignment = Alignment.TopCenter) {
            LazyColumn(
                modifier = Modifier.widthIn(max = 720.dp).fillMaxSize(),
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, bottom = 32.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                item(key = "summary") {
                    Card(
                        shape = MaterialTheme.shapes.extraLarge,
                        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        if (ui.summary == null) {
                            Row(Modifier.padding(20.dp), verticalAlignment = Alignment.CenterVertically) {
                                LoadingIndicator(Modifier.size(40.dp).semantics { contentDescription = "Measuring" })
                                Spacer(Modifier.width(16.dp))
                                Text("Measuring…", style = MaterialTheme.typography.titleMedium)
                            }
                        } else {
                            Column(Modifier.padding(20.dp)) {
                                Text(ui.summary, style = MaterialTheme.typography.bodyLarge)
                                LinearProgressIndicator(
                                    progress = { ui.used.coerceIn(0f, 1f) },
                                    modifier = Modifier.fillMaxWidth().padding(top = 14.dp).semantics {
                                        contentDescription = "The phone's storage in use"
                                    },
                                )
                            }
                        }
                    }
                }
                ui.progress?.let { p -> item(key = "progress") { ProgressCard(p, VIcons.Storage) } }
                ui.sections.forEach { section ->
                    item(key = "h:${section.title}") { SectionTitle(section.title) }
                    if (section.rows.isEmpty()) {
                        item(key = "e:${section.title}") {
                            Text(
                                section.empty,
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                modifier = Modifier.padding(horizontal = 8.dp),
                            )
                        }
                    }
                    section.all?.let { all ->
                        item(key = "a:${section.title}") {
                            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) { StorageAction(all, ui.busy) }
                        }
                    }
                    items(section.rows, key = { "${section.title}:${it.title}" }) { StorageRowCard(it, ui.busy) }
                }
                ui.reset?.let { reset ->
                    item(key = "reset") {
                        OutlinedCard(
                            shape = MaterialTheme.shapes.extraLarge,
                            border = BorderStroke(1.dp, MaterialTheme.colorScheme.error),
                            modifier = Modifier.fillMaxWidth().padding(top = 24.dp),
                        ) {
                            Column(Modifier.padding(20.dp)) {
                                Text("Reset everything", style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.error)
                                Text(
                                    ui.resetText,
                                    style = MaterialTheme.typography.bodyMedium,
                                    color = MaterialTheme.colorScheme.error,
                                    modifier = Modifier.padding(top = 6.dp, bottom = 10.dp),
                                )
                                StorageAction(reset, ui.busy)
                            }
                        }
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun StorageRowCard(row: StorageRow, busy: Boolean) {
    Card(
        shape = MaterialTheme.shapes.large,
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(start = 20.dp, end = 20.dp, top = 16.dp, bottom = 10.dp)) {
            Text(row.title, style = MaterialTheme.typography.titleMedium)
            Text(row.detail, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 4.dp))
            if (row.blocked.isNotEmpty()) {
                Text(
                    row.blocked,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                    modifier = Modifier.padding(top = 6.dp),
                )
            }
            FlowRow(
                Modifier.fillMaxWidth().padding(top = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End),
            ) { row.buttons.forEach { StorageAction(it, busy) } }
        }
    }
}

@Composable
private fun StorageAction(button: StorageButton, busy: Boolean) {
    val enabled = button.enabled && !busy
    if (button.danger) {
        OutlinedButton(
            onClick = button.action,
            enabled = enabled,
            colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
        ) { Text(button.label) }
    } else {
        FilledTonalButton(onClick = button.action, enabled = enabled) { Text(button.label) }
    }
}
