package com.vphone.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearWavyProgressIndicator
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedListItem
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.util.Locale

const val MIB = 1024L * 1024

/** "3.4 MB/s"; empty when no rate is known. */
fun rateText(bytesPerSec: Long): String =
    if (bytesPerSec <= 0) "" else String.format(Locale.US, "%.1f MB/s", bytesPerSec / MIB.toDouble())

/** "about 2 min 05 s left"; empty when it cannot be told. */
fun leftText(secs: Long): String = when {
    secs <= 0 -> ""
    secs >= 3600 -> String.format(Locale.US, "about %d h %02d min left", secs / 3600, secs % 3600 / 60)
    secs >= 60 -> String.format(Locale.US, "about %d min %02d s left", secs / 60, secs % 60)
    else -> "about $secs s left"
}

class DialogAction(val label: String, val action: () -> Unit = {})

/**
 * One dialog, as the screens' logic describes it. Its message is shown whole, scrolls, and can be
 * selected; with [copyText] it also has a Copy button, which every error has.
 */
class DialogSpec(
    val title: String,
    val message: String = "",
    val confirm: DialogAction? = DialogAction("OK"),
    val dismiss: DialogAction? = null,
    val neutral: DialogAction? = null,
    /** Choices, each of which closes the dialog. */
    val items: List<DialogAction> = emptyList(),
    val copyText: String? = null,
    val isError: Boolean = false,
    val monospace: Boolean = false,
    /** The confirm button works only once this word is typed. */
    val typeWord: String = "",
    /** A tick box, whose state goes to [onConfirmChecked] instead of [confirm]'s action. */
    val checkLabel: String? = null,
    val onConfirmChecked: ((Boolean) -> Unit)? = null,
) {
    companion object {
        /** An error: whole, selectable, and with Copy. */
        fun error(title: String, message: String) =
            DialogSpec(title, message, copyText = message, isError = true)
    }
}

/** Holds the dialog a screen shows; the screen's logic sets it, [AppDialog] draws it. */
class DialogHost {
    var current by mutableStateOf<DialogSpec?>(null)

    fun show(spec: DialogSpec) {
        current = spec
    }
}

@OptIn(ExperimentalLayoutApi::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun AppDialog(spec: DialogSpec, onCopy: (String) -> Unit, onDismiss: () -> Unit) {
    var typed by remember(spec) { mutableStateOf("") }
    var checked by remember(spec) { mutableStateOf(false) }
    fun run(action: () -> Unit) {
        onDismiss()
        action()
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        icon = if (spec.isError) {
            { Icon(VIcons.Error, contentDescription = "Error", tint = MaterialTheme.colorScheme.error) }
        } else {
            null
        },
        title = { Text(spec.title) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                if (spec.message.isNotEmpty()) {
                    SelectionContainer {
                        Text(
                            spec.message,
                            style = if (spec.monospace) {
                                MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace, fontSize = 11.sp, lineHeight = 15.sp)
                            } else {
                                MaterialTheme.typography.bodyMedium
                            },
                        )
                    }
                }
                if (spec.items.isNotEmpty()) {
                    Column(
                        Modifier.padding(top = if (spec.message.isEmpty()) 0.dp else 12.dp),
                        verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap),
                    ) {
                        spec.items.forEachIndexed { i, item ->
                            SegmentedListItem(
                                onClick = { run(item.action) },
                                shapes = ListItemDefaults.segmentedShapes(i, spec.items.size),
                                colors = ListItemDefaults.segmentedColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHighest),
                            ) { Text(item.label) }
                        }
                    }
                }
                if (spec.checkLabel != null) {
                    Row(
                        Modifier.fillMaxWidth().padding(top = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Checkbox(checked = checked, onCheckedChange = { checked = it })
                        Text(spec.checkLabel, style = MaterialTheme.typography.bodyMedium)
                    }
                }
                if (spec.typeWord.isNotEmpty()) {
                    OutlinedTextField(
                        value = typed,
                        onValueChange = { typed = it },
                        label = { Text("Type ${spec.typeWord} to continue") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Characters),
                        modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
                    )
                }
            }
        },
        confirmButton = {
            FlowRow(horizontalArrangement = Arrangement.spacedBy(4.dp, Alignment.End)) {
                spec.copyText?.let { text -> TextButton(onClick = { onCopy(text) }) { Text("Copy") } }
                spec.neutral?.let { a -> TextButton(onClick = { run(a.action) }) { Text(a.label) } }
                spec.dismiss?.let { a -> TextButton(onClick = { run(a.action) }) { Text(a.label) } }
                spec.confirm?.let { a ->
                    TextButton(
                        enabled = spec.typeWord.isEmpty() || typed == spec.typeWord,
                        onClick = {
                            val ticked = spec.onConfirmChecked
                            if (ticked != null) run { ticked(checked) } else run(a.action)
                        },
                    ) { Text(a.label) }
                }
            }
        },
    )
}

/** What a progress card shows. `fraction` is null while the size of the work is not known. */
data class ProgressUi(
    val title: String,
    val detail: String = "",
    val fraction: Float? = null,
    /** Short facts under the bar: the amount done, the speed, the time left. */
    val stats: List<String> = emptyList(),
    val cancelLabel: String? = null,
)

/** Something under way: a wavy bar, what has been done of it, how fast, and how long is left. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun ProgressCard(
    ui: ProgressUi,
    icon: ImageVector,
    modifier: Modifier = Modifier,
    onCancel: () -> Unit = {},
    containerColor: Color = MaterialTheme.colorScheme.secondaryContainer,
    extra: @Composable () -> Unit = {},
) {
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(containerColor = containerColor),
    ) {
        Column(Modifier.padding(20.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(icon, contentDescription = null, modifier = Modifier.size(24.dp))
                Spacer(Modifier.width(12.dp))
                Text(ui.title, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                if (ui.fraction != null) {
                    Text("${(ui.fraction * 100).toInt()}%", style = MaterialTheme.typography.titleMedium)
                }
            }
            if (ui.detail.isNotEmpty()) {
                Text(ui.detail, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 6.dp))
            }
            val described = Modifier.fillMaxWidth().padding(top = 14.dp).semantics { contentDescription = ui.title }
            if (ui.fraction != null) {
                LinearWavyProgressIndicator(progress = { ui.fraction.coerceIn(0f, 1f) }, modifier = described)
            } else {
                LinearWavyProgressIndicator(modifier = described)
            }
            if (ui.stats.isNotEmpty()) StatRow(ui.stats, Modifier.padding(top = 12.dp))
            extra()
            if (ui.cancelLabel != null) {
                Row(Modifier.fillMaxWidth().padding(top = 4.dp), horizontalArrangement = Arrangement.End) {
                    TextButton(onClick = onCancel) { Text(ui.cancelLabel) }
                }
            }
        }
    }
}

/** Short facts as pills that wrap. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun StatRow(stats: List<String>, modifier: Modifier = Modifier, color: Color = MaterialTheme.colorScheme.surfaceContainerHighest) {
    FlowRow(
        modifier = modifier,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        stats.filter { it.isNotEmpty() }.forEach { s ->
            Surface(shape = RoundedCornerShape(50), color = color) {
                Text(s, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(horizontal = 10.dp, vertical = 5.dp))
            }
        }
    }
}

/** An error, whole: the text can be selected, and Copy takes all of it. */
@Composable
fun ErrorCard(
    title: String,
    message: String,
    modifier: Modifier = Modifier,
    onCopy: () -> Unit,
    copyLabel: String = "Copy",
    actions: @Composable () -> Unit = {},
) {
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        ),
    ) {
        Column(Modifier.padding(start = 20.dp, end = 20.dp, top = 20.dp, bottom = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(VIcons.Error, contentDescription = "Error", modifier = Modifier.size(24.dp))
                Spacer(Modifier.width(12.dp))
                Text(title, style = MaterialTheme.typography.titleMedium)
            }
            SelectionContainer {
                Text(message, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 8.dp))
            }
            Row(Modifier.fillMaxWidth().heightIn(min = 48.dp), horizontalArrangement = Arrangement.End) {
                actions()
                TextButton(onClick = onCopy) { Text(copyLabel) }
            }
        }
    }
}

@Composable
fun SectionTitle(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = modifier.padding(start = 8.dp, top = 20.dp, bottom = 8.dp),
    )
}
