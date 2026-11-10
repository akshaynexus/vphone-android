package com.vphone.android

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Handler
import android.os.Looper

/**
 * The audio focus for the VM's sound. QEMU plays iOS's sound through AAudio, which takes no
 * part in the focus: this asks for it once iOS has played for a couple of seconds (a keyboard
 * click or an alert is over before that and takes nobody's music away), and gives it back a
 * few seconds after iOS falls silent.
 *
 * While another app holds the focus (a call, or a player the user started) what the phone plays
 * of the VM is silenced in QEMU; iOS itself plays on, as it does not know. After a loss for good
 * the focus is asked for again only for a new sound, so the VM does not take it straight back.
 * Android lowers the VM's volume by itself where the other app only asks for that.
 *
 * [tick] runs once a second on the service's monitor thread while a VM is up.
 */
object VmAudio {
    private const val PLAYING_BEFORE_FOCUS = 2
    private const val SILENT_BEFORE_RELEASE = 5

    private var request: AudioFocusRequest? = null
    private var playingFor = 0
    private var silentFor = 0
    /** Another app took the focus for good while this sound played. */
    private var lost = false
    private var muted = false

    private fun say(text: String) = Native.logLine("audio focus: $text")

    private fun mute(muted: Boolean, why: String) {
        this.muted = muted
        Native.setAudioMuted(muted)
        say(if (muted) "$why; the VM's sound is silenced on this phone" else "$why; the VM is heard again")
    }

    private val listener = AudioManager.OnAudioFocusChangeListener { change ->
        synchronized(this) {
            when (change) {
                AudioManager.AUDIOFOCUS_GAIN -> mute(false, "back")
                AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> mute(true, "another app has it for a while")
                AudioManager.AUDIOFOCUS_LOSS -> {
                    lost = true
                    request = null
                    mute(true, "another app took it")
                }
            }
        }
    }

    @Synchronized
    fun tick(context: Context) {
        val playing = Native.audioPlaying()
        if (playing) {
            playingFor++
            silentFor = 0
        } else {
            silentFor++
            playingFor = 0
        }
        val audio = context.getSystemService(AudioManager::class.java)
        if (playing && request == null && !lost && playingFor >= PLAYING_BEFORE_FOCUS) {
            val wanted = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build(),
                )
                .setOnAudioFocusChangeListener(listener, Handler(Looper.getMainLooper()))
                .build()
            if (audio.requestAudioFocus(wanted) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
                request = wanted
                say("taken, iOS is playing")
            } else {
                // A call, most likely: asked again for the next sound.
                lost = true
                mute(true, "refused")
            }
        }
        if (!playing && silentFor >= SILENT_BEFORE_RELEASE && (request != null || lost || muted)) {
            release(audio, "given back, iOS is silent")
        }
    }

    private fun release(audio: AudioManager, why: String) {
        request?.let { audio.abandonAudioFocusRequest(it) }
        request = null
        lost = false
        if (muted) {
            muted = false
            Native.setAudioMuted(false)
        }
        say(why)
    }

    /** The VM ended. */
    @Synchronized
    fun stop(context: Context) {
        if (request != null || lost || muted) release(context.getSystemService(AudioManager::class.java), "given back, the VM ended")
        playingFor = 0
        silentFor = 0
    }
}
