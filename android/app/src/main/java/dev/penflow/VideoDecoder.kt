package dev.penflow

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

/**
 * Async hardware decoder rendering directly into a Surface.
 *
 * Async mode (callbacks) keeps the decode pipeline pulling buffers as fast
 * as the codec produces them, with no thread context switches in the hot
 * path. Releasing output buffers with `render = true` hands frames straight
 * to the [Surface]'s buffer queue — zero CPU copy.
 *
 * Codec mime is chosen at construct time from the handshake's `HELLO_PC.codec`:
 * `video/avc` or `video/hevc`. The csd-0 layout is opaque to us — MediaCodec
 * consumes whatever NVENC produced.
 *
 * ## Latency-sensitive tuning (design.md §10.2 / HANDOFF §1.3)
 *
 * - `KEY_LOW_LATENCY = 1` — Android 11+ canonical low-latency hint.
 * - **Qualcomm**: `KEY_OPERATING_RATE = Short.MAX_VALUE`, do NOT set
 *   `KEY_PRIORITY = 0`. The combination crashes Adreno 620 (Snapdragon
 *   765G — Mi 10 lite 5G, Redmi K30i 5G) per moonlight-android's
 *   `MediaCodecHelper.java:482`. The dev rig (MovinkPad's Adreno 720) is
 *   fine, but anyone with a 765G-class device used to SIGSEGV on connect.
 * - **Non-Qualcomm**: `KEY_PRIORITY = 0` is safe; do NOT set
 *   `KEY_OPERATING_RATE`.
 * - `vendor.qti-ext-dec-low-latency.enable = 1` — Qualcomm-private
 *   low-latency pipeline switch.
 * - `vendor.qti-ext-dec-picture-order.enable = 1` — Qualcomm-private flag
 *   that disables HEVC reorder buffering. Saves 5–10 ms per frame on
 *   Qualcomm chips (moonlight-android finding).
 *
 * ## Frame pacing (design.md §10.6 — MIN_LATENCY mode)
 *
 * Older code unconditionally called `releaseOutputBuffer(index, true)` which
 * always renders. Under load that can pile up multiple buffers per vsync.
 * The async-safe MIN_LATENCY pattern below coalesces callback bursts to the
 * newest output index, releasing older indices with `render = false`, and
 * posts the render release on the codec handler so SurfaceFlinger can drop
 * late buffers automatically.
 *
 * ## Input-buffer feeding
 *
 * The naive pattern (queue an empty buffer back when our packet queue is empty)
 * makes MediaCodec spin a no-op decode pass that adds a frame of latency. We
 * instead **park** the buffer index until a packet actually arrives, then feed
 * it directly. This eliminates the empty-buffer round-trip.
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

    /** Owning codec callbacks; same handler is reused for posted render releases. */
    private val codecThread = HandlerThread("video-codec").apply { start() }
    private val codecHandler = Handler(codecThread.looper)

    // Single mutex protecting both INPUT queues. Producer (network thread)
    // calls feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    private val lock = Any()
    private val pendingData = ArrayDeque<ByteArray>()
    private val parkedIndices = ArrayDeque<Int>()

    // OUTPUT-side MIN_LATENCY state — drains to newest output index, drops the rest.
    private val outputLock = Any()
    private var newestOutputIndex: Int? = null
    private var renderPosted = false

    fun start() {
        // Identify the chosen codec's vendor so we apply the right vendor-key
        // ladder (design.md §10.2). codec.name reflects the codec we got from
        // createDecoderByType — known before we call configure(), which is
        // when we need the format flags ready.
        val codecName = codec.name.lowercase()
        val isQualcomm = codecName.startsWith("omx.qcom.") || codecName.startsWith("c2.qti.")
        val isKirin = codecName.startsWith("omx.hisi.") || codecName.startsWith("c2.hisi.")
        val isExynos = codecName.startsWith("omx.exynos.") || codecName.startsWith("c2.exynos.")

        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd0))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)

            if (isQualcomm) {
                // Adreno 620 fix: do NOT set KEY_PRIORITY=0 alongside
                // KEY_OPERATING_RATE on Qualcomm. Use moonlight's value
                // (Short.MAX_VALUE) for KEY_OPERATING_RATE alone.
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
                    // Disables HEVC reorder buffering on Qualcomm chips —
                    // saves 5–10 ms of decode delay per frame.
                    setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
                }
            } else {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    setInteger(MediaFormat.KEY_PRIORITY, 0)
                }
                // Best-effort vendor low-latency hints for non-Qualcomm
                // SoCs. Unknown vendor keys are silently ignored by codecs
                // that don't recognise them; we set them anyway because the
                // ones the codec DOES recognise carry meaningful latency
                // wins.
                if (isKirin && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    setInteger(
                        "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
                        1,
                    )
                }
                if (isExynos && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    setInteger("vendor.rtc-ext-dec-low-latency.enable", 1)
                }
            }
        }

        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                val data: ByteArray? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
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

                // MIN_LATENCY drain: keep only the newest index, release any
                // older one with render=false. Post the render release on the
                // codec handler so SurfaceFlinger can supersede it if a newer
                // buffer arrives in the same vsync window.
                var shouldPostRender = false
                val dropped: Int? = synchronized(outputLock) {
                    val d = newestOutputIndex
                    newestOutputIndex = index
                    if (!renderPosted) {
                        renderPosted = true
                        shouldPostRender = true
                    }
                    d
                }
                dropped?.let { c.releaseOutputBuffer(it, false) }

                if (shouldPostRender) {
                    codecHandler.post {
                        val toRender: Int? = synchronized(outputLock) {
                            val chosen = newestOutputIndex
                            newestOutputIndex = null
                            renderPosted = false
                            chosen
                        }
                        toRender?.let { idx ->
                            // Pass System.nanoTime() as the render PTS so
                            // SurfaceFlinger schedules for the next vsync
                            // and drops late buffers if superseded under load.
                            c.releaseOutputBuffer(idx, System.nanoTime())
                        }
                    }
                }

                onDecoded(decodedNs)
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(TAG, "decoder error", e)
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "decoder output format: $format")
            }
        }, codecHandler)

        codec.configure(format, surface, null, 0)
        codec.start()
        Log.i(
            TAG,
            "started $mime decoder ${width}x${height}@${fps} on $codecName " +
                "(qualcomm=$isQualcomm)"
        )
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
        codecThread.quitSafely()
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
