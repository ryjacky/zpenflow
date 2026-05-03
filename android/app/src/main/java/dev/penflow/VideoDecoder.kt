package dev.penflow

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

/**
 * Async hardware decoder rendering directly into a Surface.
 *
 * **Status:** mirrors the predecessor's working configuration verbatim. The
 * Wave-3 over-eager additions (KEY_OPERATING_RATE = Short.MAX_VALUE for
 * Qualcomm, vendor.qti-ext-dec-picture-order.enable, the newest-buffer-wins
 * MIN_LATENCY render pattern, KEY_COLOR_FORMAT) all collectively pushed
 * c2.qti.hevc.decoder onto a degraded path where it logged
 * `configureIntf failed 95` + `unsupported pixel format` and silently
 * stopped feeding the SurfaceView. The dev rig (MovinkPad / Adreno 720,
 * Snapdragon 8s Gen 3) works fine with the predecessor's exact knobs.
 *
 * Deliberately deferred for a follow-up:
 *   - §10.2 Adreno 620 SIGSEGV fix → must gate on the actual chip
 *     (`Build.SOC_MANUFACTURER == "QTI"` && Adreno 620 indicators) rather
 *     than blanket "is qualcomm".
 *   - §10.3 codec recovery ladder + §10.4 watchdog + §10.5
 *     surface-destroyed handler — robustness work, not blocking demo.
 *   - §10.6 MIN_LATENCY render via `releaseOutputBuffer(idx, nanoTime())` —
 *     useful at high frame rates but breaks on this codec; revisit with a
 *     vendor-specific gate.
 *
 * Codec mime is chosen at construct time from the handshake's
 * `HELLO_PC.codec`: `video/avc` or `video/hevc`. The csd-0 layout is
 * opaque to us — MediaCodec consumes whatever the PC encoder produced.
 */
class VideoDecoder(
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val codecId: Byte,
    private val surface: Surface,
    private val csd0: ByteArray,
    private val onDecoded: (decodedNs: Long) -> Unit = {},
) {

    private val mime: String = mimeFor(codecId)
    private val codec: MediaCodec = MediaCodec.createDecoderByType(mime)

    // Single mutex protecting both queues. Producer (network thread) calls
    // feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    private val lock = Any()
    private val pendingData = ArrayDeque<ByteArray>()
    private val parkedIndices = ArrayDeque<Int>()

    fun start() {
        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd0))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)

            // Latency-sensitive operation: tell the codec we'll feed at up to
            // 240 fps so it picks a high clock domain rather than throttling.
            // This combination (240 + KEY_PRIORITY=0) crashes Adreno 620
            // (Snapdragon 765G); the dev rig (Adreno 720, Snapdragon 8s Gen 3)
            // is fine. Re-add the chip-specific branch in a follow-up.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                setInteger(MediaFormat.KEY_OPERATING_RATE, 240)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
            }

            // Qualcomm-private low-latency flag. Setting on non-Qualcomm
            // devices is a silent no-op (MediaFormat doesn't validate vendor
            // keys against the codec).
            setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
        }

        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                val data: ByteArray? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
                        // No data yet — park this index for feed() to pick up.
                        parkedIndices.addLast(index)
                        null
                    }
                }
                if (data != null) {
                    feedBuffer(c, index, data)
                }
            }

            override fun onOutputBufferAvailable(
                c: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo
            ) {
                val decodedNs = System.nanoTime()
                c.releaseOutputBuffer(index, true)
                onDecoded(decodedNs)
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(TAG, "decoder error: code=${e.errorCode} diagnostic=${e.diagnosticInfo} recoverable=${e.isRecoverable} transient=${e.isTransient}", e)
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "decoder output format: $format")
            }
        })

        codec.configure(format, surface, null, 0)
        codec.start()
        Log.i(TAG, "started $mime decoder ${width}x${height}@${fps} on ${codec.name} (operating_rate=240)")
    }

    /** Submit a coded video access unit (Annex-B framed). */
    fun feed(coded: ByteArray) {
        val parkedIndex: Int? = synchronized(lock) {
            if (parkedIndices.isNotEmpty()) {
                parkedIndices.removeFirst()
            } else {
                pendingData.addLast(coded)
                null
            }
        }
        if (parkedIndex != null) {
            feedBuffer(codec, parkedIndex, coded)
        }
    }

    private fun feedBuffer(c: MediaCodec, index: Int, data: ByteArray) {
        val buf = c.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(data)
        c.queueInputBuffer(index, 0, data.size, System.nanoTime() / 1000, 0)
    }

    fun stop() {
        try {
            codec.stop()
        } catch (_: IllegalStateException) {
        }
        codec.release()
    }

    companion object {
        private const val TAG = "VideoDecoder"

        fun mimeFor(codec: Byte): String = when (codec) {
            Protocol.CODEC_H264 -> "video/avc"
            Protocol.CODEC_HEVC -> "video/hevc"
            else -> throw IllegalArgumentException("unknown codec id: $codec")
        }
    }
}
